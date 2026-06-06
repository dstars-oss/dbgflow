use super::{SessionId, SessionState};
use crate::artifacts::{ArtifactManager, ArtifactRef, CommandArtifactRecord};
use crate::backend::{CreateBackendSession, DebugTarget};
use crate::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use crate::policy::CommandPolicy;
use crate::session::worker::{ProcessWorkerLauncher, SessionWorker, SessionWorkerLauncher};
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLOSE_OPERATION_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub target: DebugTarget,
    pub startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub backend: String,
    pub backend_session_id: Option<String>,
    pub target: DebugTarget,
    pub state: SessionState,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub warnings: Vec<String>,
    pub current_operation: Option<String>,
    pub last_operation: Option<OperationSummary>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSummary {
    pub command_id: String,
    pub command: String,
    pub status: OperationStatus,
    pub started_at_unix_ms: u128,
    pub finished_at_unix_ms: Option<u128>,
    pub duration_ms: Option<u128>,
    pub artifact: Option<ArtifactRef>,
    pub error: Option<String>,
    pub output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationStatus {
    Running,
    CancelRequested,
    Canceled,
    Finished,
    Failed,
}

#[derive(Clone)]
pub struct SessionManager {
    worker_launcher: Arc<dyn SessionWorkerLauncher>,
    workers: Arc<WorkerRegistry>,
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    operation_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    artifacts: ArtifactManager,
    command_policy: CommandPolicy,
    event_subscribers: Arc<Mutex<Vec<mpsc::Sender<SessionId>>>>,
    logger: Arc<dyn LogSink>,
}

#[derive(Default)]
struct WorkerRegistry {
    workers: Mutex<HashMap<SessionId, Arc<dyn SessionWorker>>>,
}

impl WorkerRegistry {
    fn get(&self, session_id: SessionId) -> Option<Arc<dyn SessionWorker>> {
        self.workers
            .lock()
            .ok()
            .and_then(|workers| workers.get(&session_id).cloned())
    }

    fn insert(&self, session_id: SessionId, worker: Arc<dyn SessionWorker>) -> Result<()> {
        self.workers
            .lock()
            .map_err(|_| DbgFlowError::Backend("session worker map poisoned".to_string()))?
            .insert(session_id, worker);
        Ok(())
    }

    fn remove(&self, session_id: SessionId) -> Option<Arc<dyn SessionWorker>> {
        self.workers
            .lock()
            .ok()
            .and_then(|mut workers| workers.remove(&session_id))
    }
}

impl Drop for WorkerRegistry {
    fn drop(&mut self) {
        let workers = self
            .workers
            .get_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        for (session_id, worker) in workers {
            let _ = worker.kill(&format!("session_manager_drop:{session_id}"));
        }
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self::with_artifact_root("artifacts")
    }

    pub fn with_artifact_root(artifact_root: impl Into<PathBuf>) -> Self {
        Self::with_artifact_root_and_logger(artifact_root, noop_logger())
    }

    pub fn with_artifact_root_and_logger(
        artifact_root: impl Into<PathBuf>,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self::with_worker_launcher_and_logger(
            Arc::new(ProcessWorkerLauncher::new()),
            artifact_root,
            logger,
        )
    }

    pub fn with_worker_launcher(
        worker_launcher: Arc<dyn SessionWorkerLauncher>,
        artifact_root: impl Into<PathBuf>,
    ) -> Self {
        Self::with_worker_launcher_and_logger(worker_launcher, artifact_root, noop_logger())
    }

    pub fn with_worker_launcher_and_logger(
        worker_launcher: Arc<dyn SessionWorkerLauncher>,
        artifact_root: impl Into<PathBuf>,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self {
            worker_launcher,
            workers: Arc::new(WorkerRegistry::default()),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
            artifacts: ArtifactManager::new(artifact_root),
            command_policy: CommandPolicy::default_query_policy(),
            event_subscribers: Arc::new(Mutex::new(Vec::new())),
            logger,
        }
    }

    pub fn with_default_worker_at(artifact_root: impl Into<PathBuf>) -> Self {
        Self::with_artifact_root(artifact_root)
    }

    pub fn with_default_worker_at_and_logger(
        artifact_root: impl Into<PathBuf>,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self::with_artifact_root_and_logger(artifact_root, logger)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?
            .values()
            .cloned()
            .collect::<Vec<_>>();

        sessions.sort_by_key(|session| session.created_at_unix_ms);
        Ok(sessions)
    }

    pub fn create_session(&self, mut request: CreateSession) -> Result<Session> {
        request.target = self.validate_target(request.target)?;

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;

        if let Some(existing) = sessions
            .values()
            .find(|session| session.target == request.target && session.state.is_reusable())
            .cloned()
        {
            return Ok(existing);
        }

        let now = now_unix_ms();
        let session = Session {
            id: SessionId::new(),
            backend: "worker".to_string(),
            backend_session_id: None,
            target: request.target.clone(),
            state: SessionState::Starting,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            warnings: Vec::new(),
            current_operation: Some("create_session".to_string()),
            last_operation: None,
            error: None,
        };

        sessions.insert(session.id, session.clone());
        drop(sessions);

        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("session lock map poisoned".to_string()))?
            .insert(session.id, Arc::new(Mutex::new(())));

        self.log(
            LogEvent::new(LogLevel::Info, "session", "create_session_started")
                .session_id(session.id)
                .operation("create_session")
                .field("backend", "worker")
                .field("target", &session.target),
        );
        if let Some(startup_timeout_ms) = request.startup_timeout_ms {
            self.log(
                LogEvent::new(
                    LogLevel::Warn,
                    "session",
                    "deprecated_startup_timeout_ignored",
                )
                .session_id(session.id)
                .operation("create_session")
                .field("startup_timeout_ms", startup_timeout_ms),
            );
        }

        self.spawn_worker_startup(session.id, request.target);
        self.notify_session_updated(session.id);

        Ok(session)
    }

    pub fn subscribe_session_updates(&self) -> mpsc::Receiver<SessionId> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.event_subscribers.lock() {
            subscribers.push(tx);
        }
        rx
    }

    pub fn query_session(&self, session_id: SessionId) -> Result<Session> {
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<Session> {
        let operation_lock = self.operation_lock(session_id)?;
        let has_worker = self.worker(session_id).is_some();
        let (session, should_cancel) = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
            let session = sessions
                .get_mut(&session_id)
                .ok_or(DbgFlowError::SessionNotFound(session_id))?;

            if session.state == SessionState::Closed {
                return Err(DbgFlowError::SessionClosed(session_id));
            }
            if session.state == SessionState::Closing {
                return Ok(session.clone());
            }

            let should_cancel = session.current_operation.is_some();
            session.state = if has_worker || should_cancel {
                SessionState::Closing
            } else {
                SessionState::Closed
            };
            session.updated_at_unix_ms = now_unix_ms();
            session.current_operation = None;
            if should_cancel {
                if let Some(operation) = session.last_operation.as_mut() {
                    if operation.status == OperationStatus::Running {
                        operation.status = OperationStatus::CancelRequested;
                    }
                }
            }
            self.log(
                LogEvent::new(LogLevel::Info, "session", "close_session_requested")
                    .session_id(session_id)
                    .field("backend", session.backend.clone())
                    .field("had_worker", has_worker)
                    .field("cancel_requested", should_cancel),
            );
            (session.clone(), should_cancel)
        };
        self.notify_session_updated(session_id);

        if should_cancel {
            if let Some(worker) = self.worker(session_id) {
                self.log(
                    LogEvent::new(LogLevel::Info, "session", "worker_cancel_requested")
                        .session_id(session_id),
                );
                match worker.kill("close_session_cancel") {
                    Ok(()) => self.log(
                        LogEvent::new(LogLevel::Info, "session", "worker_cancel_finished")
                            .session_id(session_id),
                    ),
                    Err(error) => self.log(
                        LogEvent::new(LogLevel::Warn, "session", "worker_cancel_failed")
                            .session_id(session_id)
                            .error(error.to_string()),
                    ),
                }
            }
        }

        if session.state == SessionState::Closing {
            let manager = self.clone();
            thread::spawn(move || {
                let lock_started = Instant::now();
                let _operation_guard = loop {
                    match operation_lock.try_lock() {
                        Ok(guard) => break guard,
                        Err(TryLockError::WouldBlock)
                            if lock_started.elapsed() >= CLOSE_OPERATION_WAIT_TIMEOUT =>
                        {
                            if let Some(worker) = manager.remove_worker(session_id) {
                                let _ = worker.kill("close_operation_wait_timed_out");
                            }
                            let error = DbgFlowError::Backend(
                                "session operation did not finish after close cancellation"
                                    .to_string(),
                            );
                            manager.log(
                                LogEvent::new(
                                    LogLevel::Error,
                                    "session",
                                    "worker_close_wait_timed_out",
                                )
                                .session_id(session_id)
                                .error(error.to_string()),
                            );
                            manager.finish_worker_close(session_id, Err(error));
                            return;
                        }
                        Err(TryLockError::WouldBlock) => {
                            thread::sleep(Duration::from_millis(50));
                        }
                        Err(TryLockError::Poisoned(_)) => {
                            let error = DbgFlowError::Backend(
                                "session operation lock poisoned".to_string(),
                            );
                            manager.finish_worker_close(session_id, Err(error));
                            return;
                        }
                    }
                };

                manager.log(
                    LogEvent::new(LogLevel::Info, "session", "worker_close_started")
                        .session_id(session_id),
                );
                let close_result = manager
                    .remove_worker(session_id)
                    .map(|worker| worker.close())
                    .unwrap_or(Ok(()));
                manager.finish_worker_close(session_id, close_result);
            });
        }

        Ok(session)
    }

    pub fn eval(&self, request: EvalSession) -> Result<EvalSessionResult> {
        self.command_policy.check_command(&request.command)?;
        let is_run_control = self.command_policy.is_run_control_command(&request.command);

        let operation_lock = self.operation_lock(request.session_id)?;
        let _operation_guard = operation_lock
            .lock()
            .map_err(|_| DbgFlowError::Backend("session operation lock poisoned".to_string()))?;

        let session = self.query_session(request.session_id)?;
        if session.state.is_terminal() {
            return Err(DbgFlowError::SessionClosed(request.session_id));
        }
        if !matches!(session.state, SessionState::Ready | SessionState::Break) {
            return Err(DbgFlowError::Backend(format!(
                "session is not ready: {:?}",
                session.state
            )));
        }
        let worker = self.worker(request.session_id).ok_or_else(|| {
            DbgFlowError::Backend("session worker is not initialized".to_string())
        })?;

        let started_at_unix_ms = now_unix_ms();
        let started = Instant::now();
        let command_id = SessionId::new().to_string();
        let command = request.command.clone();
        self.start_operation(
            request.session_id,
            if is_run_control {
                SessionState::Running
            } else {
                session.state
            },
            OperationSummary {
                command_id: command_id.clone(),
                command: command.clone(),
                status: OperationStatus::Running,
                started_at_unix_ms,
                finished_at_unix_ms: None,
                duration_ms: None,
                artifact: None,
                error: None,
                output_bytes: None,
            },
        )?;
        if let Some(timeout_ms) = request.timeout_ms {
            self.log(
                LogEvent::new(LogLevel::Warn, "session", "deprecated_eval_timeout_ignored")
                    .session_id(request.session_id)
                    .backend_session_id(
                        session
                            .backend_session_id
                            .clone()
                            .unwrap_or_else(|| "worker".to_string()),
                    )
                    .operation(command.clone())
                    .field("command_id", command_id.clone())
                    .field("timeout_ms", timeout_ms),
            );
        }
        self.log(
            LogEvent::new(LogLevel::Info, "session", "eval_started")
                .session_id(request.session_id)
                .operation(command.clone())
                .field("command_id", command_id.clone())
                .field("backend", session.backend.clone())
                .field("is_run_control", is_run_control),
        );
        let backend_result = worker.execute(command.clone());
        let duration_ms = started.elapsed().as_millis();

        let backend_result = match backend_result {
            Ok(result) => result,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "session", "eval_failed")
                        .session_id(request.session_id)
                        .operation(command.clone())
                        .duration_ms(duration_ms)
                        .field("command_id", command_id.clone())
                        .error(error.to_string()),
                );
                self.finish_operation(
                    request.session_id,
                    SessionState::Error,
                    OperationStatus::Failed,
                    None,
                    Some(error.to_string()),
                    None,
                    duration_ms,
                )?;
                return Err(error);
            }
        };

        let output_path = self
            .artifacts
            .root()
            .join("sessions")
            .join(request.session_id.to_string())
            .join("outputs")
            .join(format!("{command_id}.txt"));
        let artifact = match self.artifacts.write_eval_artifacts(
            request.session_id,
            &command_id,
            &CommandArtifactRecord {
                command_id: command_id.clone(),
                command: command.clone(),
                output_path,
                started_at_unix_ms,
                duration_ms,
                output_bytes: backend_result.output.len(),
            },
            &backend_result.output,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "session", "eval_artifact_failed")
                        .session_id(request.session_id)
                        .operation(command.clone())
                        .duration_ms(duration_ms)
                        .field("command_id", command_id.clone())
                        .field("output_bytes", backend_result.output.len())
                        .error(error.to_string()),
                );
                self.finish_operation(
                    request.session_id,
                    SessionState::Error,
                    OperationStatus::Failed,
                    None,
                    Some(error.to_string()),
                    Some(backend_result.output.len()),
                    duration_ms,
                )?;
                return Err(error);
            }
        };
        self.finish_operation(
            request.session_id,
            if is_run_control {
                SessionState::Break
            } else {
                session.state
            },
            OperationStatus::Finished,
            Some(artifact.clone()),
            None,
            Some(backend_result.output.len()),
            duration_ms,
        )?;

        self.log(
            LogEvent::new(LogLevel::Info, "session", "eval_finished")
                .session_id(request.session_id)
                .operation(command)
                .duration_ms(duration_ms)
                .field("command_id", command_id)
                .field("output_bytes", backend_result.output.len()),
        );

        Ok(EvalSessionResult {
            session: self.query_session(request.session_id)?,
            output: backend_result.output,
            artifact,
            warnings: backend_result.warnings,
            duration_ms,
        })
    }

    fn operation_lock(&self, session_id: SessionId) -> Result<Arc<Mutex<()>>> {
        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("session lock map poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    fn worker(&self, session_id: SessionId) -> Option<Arc<dyn SessionWorker>> {
        self.workers.get(session_id)
    }

    fn insert_worker(&self, session_id: SessionId, worker: Arc<dyn SessionWorker>) -> Result<()> {
        self.workers.insert(session_id, worker)
    }

    fn remove_worker(&self, session_id: SessionId) -> Option<Arc<dyn SessionWorker>> {
        self.workers.remove(session_id)
    }

    fn start_operation(
        &self,
        session_id: SessionId,
        state: SessionState,
        operation: OperationSummary,
    ) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        if session.state.is_terminal() || session.state == SessionState::Closing {
            return Ok(());
        }
        session.state = state;
        session.updated_at_unix_ms = now_unix_ms();
        session.current_operation = Some(operation.command.clone());
        session.last_operation = Some(operation);
        session.error = None;
        drop(sessions);
        self.notify_session_updated(session_id);
        Ok(())
    }

    fn finish_operation(
        &self,
        session_id: SessionId,
        state: SessionState,
        status: OperationStatus,
        artifact: Option<ArtifactRef>,
        error: Option<String>,
        output_bytes: Option<usize>,
        duration_ms: u128,
    ) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        if session.state.is_terminal() || session.state == SessionState::Closing {
            return Ok(());
        }
        session.state = state;
        session.updated_at_unix_ms = now_unix_ms();
        session.current_operation = None;
        session.error = error.clone();
        if let Some(operation) = session.last_operation.as_mut() {
            operation.status = status;
            operation.finished_at_unix_ms = Some(now_unix_ms());
            operation.duration_ms = Some(duration_ms);
            operation.artifact = artifact;
            operation.error = error;
            operation.output_bytes = output_bytes;
        }
        drop(sessions);
        self.notify_session_updated(session_id);
        Ok(())
    }

    fn notify_session_updated(&self, session_id: SessionId) {
        if let Ok(mut subscribers) = self.event_subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(session_id).is_ok());
        }
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }

    fn finish_worker_close(&self, session_id: SessionId, close_result: Result<()>) {
        let _ = self.remove_worker(session_id);
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&session_id) {
                match close_result {
                    Ok(()) => {
                        self.log(
                            LogEvent::new(LogLevel::Info, "session", "worker_close_finished")
                                .session_id(session_id),
                        );
                        finalize_canceled_operation(
                            session,
                            OperationStatus::Canceled,
                            "operation canceled by close_session".to_string(),
                        );
                        session.state = SessionState::Closed;
                        session.error = None;
                    }
                    Err(error) => {
                        self.log(
                            LogEvent::new(LogLevel::Error, "session", "worker_close_failed")
                                .session_id(session_id)
                                .error(error.to_string()),
                        );
                        finalize_canceled_operation(
                            session,
                            OperationStatus::Failed,
                            error.to_string(),
                        );
                        session.state = SessionState::Error;
                        session.error = Some(error.to_string());
                    }
                }
                session.updated_at_unix_ms = now_unix_ms();
                session.current_operation = None;
            }
        }
        self.notify_session_updated(session_id);
    }

    fn spawn_worker_startup(&self, session_id: SessionId, target: DebugTarget) {
        let manager = self.clone();
        thread::spawn(move || {
            let startup_started = Instant::now();
            manager.log(
                LogEvent::new(LogLevel::Info, "session", "worker_startup_thread_spawned")
                    .session_id(session_id)
                    .operation("create_session"),
            );

            let worker = match manager
                .worker_launcher
                .spawn(session_id, manager.logger.clone())
            {
                Ok(worker) => worker,
                Err(error) => {
                    manager.finish_worker_startup(
                        session_id,
                        Err(error),
                        startup_started.elapsed().as_millis(),
                    );
                    return;
                }
            };

            if let Err(error) = manager.insert_worker(session_id, worker.clone()) {
                let _ = worker.kill("insert_worker_failed");
                manager.finish_worker_startup(
                    session_id,
                    Err(error),
                    startup_started.elapsed().as_millis(),
                );
                return;
            }

            if manager.session_is_closing_or_terminal(session_id) {
                let _ = worker.kill("startup_canceled_before_create");
                manager.finish_worker_close(session_id, Ok(()));
                return;
            }

            let startup_result = worker.create_session(CreateBackendSession {
                target,
                correlation_id: Some(session_id.to_string()),
            });
            manager.finish_worker_startup(
                session_id,
                startup_result,
                startup_started.elapsed().as_millis(),
            );
        });
    }

    fn finish_worker_startup(
        &self,
        session_id: SessionId,
        startup_result: Result<crate::session::worker::WorkerSession>,
        duration_ms: u128,
    ) {
        match startup_result {
            Ok(worker_session) => {
                self.log(
                    LogEvent::new(LogLevel::Info, "session", "worker_startup_finished")
                        .session_id(session_id)
                        .backend_session_id(worker_session.backend_session_id.clone())
                        .operation("create_session")
                        .duration_ms(duration_ms)
                        .field("backend", worker_session.backend.clone()),
                );
                let mut should_close_worker = false;
                let mut closing_state = None;
                if let Ok(mut sessions) = self.sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id) {
                        if session.state.is_terminal() || session.state == SessionState::Closing {
                            should_close_worker = true;
                            closing_state = Some(session.state);
                        } else {
                            session.backend = worker_session.backend;
                            session.backend_session_id =
                                Some(worker_session.backend_session_id.clone());
                            session.state = state_for_target(&session.target);
                            session.updated_at_unix_ms = now_unix_ms();
                            session.warnings = worker_session.warnings;
                            session.current_operation = None;
                            session.error = None;
                            drop(sessions);
                            self.notify_session_updated(session_id);
                            if let Some(worker) = self.worker(session_id) {
                                self.spawn_worker_monitor(session_id, worker);
                            }
                        }
                    } else {
                        should_close_worker = true;
                    }
                }

                if should_close_worker {
                    let close_result = self
                        .remove_worker(session_id)
                        .map(|worker| worker.close())
                        .unwrap_or(Ok(()));
                    if closing_state == Some(SessionState::Closing) {
                        self.finish_worker_close(session_id, close_result);
                    }
                }
            }
            Err(error) => {
                if let Some(worker) = self.remove_worker(session_id) {
                    let _ = worker.kill("startup_failed");
                }
                self.log(
                    LogEvent::new(LogLevel::Error, "session", "worker_startup_failed")
                        .session_id(session_id)
                        .operation("create_session")
                        .duration_ms(duration_ms)
                        .error(error.to_string()),
                );
                if let Ok(mut sessions) = self.sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id) {
                        if session.state == SessionState::Closing {
                            session.state = SessionState::Closed;
                            session.updated_at_unix_ms = now_unix_ms();
                            session.current_operation = None;
                            session.error = None;
                            drop(sessions);
                            self.notify_session_updated(session_id);
                        } else if !session.state.is_terminal() {
                            session.state = SessionState::Error;
                            session.updated_at_unix_ms = now_unix_ms();
                            session.current_operation = None;
                            session.error = Some(error.to_string());
                            drop(sessions);
                            self.notify_session_updated(session_id);
                        }
                    }
                }
            }
        }
    }

    fn session_is_closing_or_terminal(&self, session_id: SessionId) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&session_id).map(|session| session.state))
            .is_some_and(|state| state == SessionState::Closing || state.is_terminal())
    }

    fn spawn_worker_monitor(&self, session_id: SessionId, worker: Arc<dyn SessionWorker>) {
        let sessions = Arc::downgrade(&self.sessions);
        let workers = Arc::downgrade(&self.workers);
        let event_subscribers = Arc::downgrade(&self.event_subscribers);
        let logger = self.logger.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(250));
            let exited = match worker.has_exited() {
                Ok(exited) => exited,
                Err(error) => {
                    mark_worker_unavailable(
                        session_id,
                        error.to_string(),
                        &sessions,
                        &workers,
                        &event_subscribers,
                        &logger,
                    );
                    return;
                }
            };
            if !exited {
                continue;
            }
            mark_worker_unavailable(
                session_id,
                "session worker exited unexpectedly".to_string(),
                &sessions,
                &workers,
                &event_subscribers,
                &logger,
            );
            return;
        });
    }

    fn validate_target(&self, target: DebugTarget) -> Result<DebugTarget> {
        match target {
            DebugTarget::Dump { path } => {
                validate_dump_target(&path).map(|path| DebugTarget::Dump { path })
            }
            DebugTarget::Attach { pid } => validate_attach_target(pid),
            DebugTarget::Launch { executable, args } => validate_launch_target(&executable, args),
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

fn state_for_target(target: &DebugTarget) -> SessionState {
    match target {
        DebugTarget::Dump { .. } | DebugTarget::Attach { .. } | DebugTarget::Launch { .. } => {
            SessionState::Break
        }
    }
}

fn finalize_canceled_operation(session: &mut Session, status: OperationStatus, error: String) {
    let now = now_unix_ms();
    if let Some(operation) = session.last_operation.as_mut() {
        if matches!(
            operation.status,
            OperationStatus::Running | OperationStatus::CancelRequested
        ) {
            operation.status = status;
            operation.finished_at_unix_ms = Some(now);
            operation.duration_ms = Some(now.saturating_sub(operation.started_at_unix_ms));
            operation.error = Some(error);
        }
    }
}

fn mark_worker_unavailable(
    session_id: SessionId,
    error: String,
    sessions: &Weak<Mutex<HashMap<SessionId, Session>>>,
    workers: &Weak<WorkerRegistry>,
    event_subscribers: &Weak<Mutex<Vec<mpsc::Sender<SessionId>>>>,
    logger: &Arc<dyn LogSink>,
) {
    let Some(sessions) = sessions.upgrade() else {
        return;
    };
    let mut should_notify = false;
    if let Ok(mut sessions) = sessions.lock() {
        if let Some(session) = sessions.get_mut(&session_id) {
            if session.state == SessionState::Closing || session.state.is_terminal() {
                return;
            }
            finalize_canceled_operation(session, OperationStatus::Failed, error.clone());
            session.state = SessionState::Error;
            session.updated_at_unix_ms = now_unix_ms();
            session.current_operation = None;
            session.error = Some(error.clone());
            should_notify = true;
        }
    }
    if should_notify {
        if let Some(workers) = workers.upgrade() {
            let _ = workers.remove(session_id);
        }
        logger.log(
            LogEvent::new(LogLevel::Error, "session", "worker_exited_unexpectedly")
                .session_id(session_id)
                .error(error),
        );
        if let Some(event_subscribers) = event_subscribers.upgrade() {
            if let Ok(mut subscribers) = event_subscribers.lock() {
                subscribers.retain(|subscriber| subscriber.send(session_id).is_ok());
            }
        }
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSession {
    pub session_id: SessionId,
    pub command: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSessionResult {
    pub session: Session,
    pub output: String,
    pub artifact: ArtifactRef,
    pub warnings: Vec<String>,
    pub duration_ms: u128,
}

fn validate_dump_target(path: &Path) -> Result<PathBuf> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(extension.as_str(), "dmp" | "mdmp" | "hdmp" | "kdmp") {
        return Err(DbgFlowError::Backend(format!(
            "dump path has unsupported extension: {}",
            path.display()
        )));
    }

    let canonical_path = path
        .canonicalize()
        .map_err(|error| DbgFlowError::Backend(format!("invalid dump path: {error}")))?;
    if !canonical_path.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "dump path is not a file: {}",
            canonical_path.display()
        )));
    }

    Ok(canonical_path)
}

fn validate_attach_target(pid: u32) -> Result<DebugTarget> {
    if pid == 0 {
        return Err(DbgFlowError::Backend(
            "attach pid must be greater than zero".to_string(),
        ));
    }
    if pid == std::process::id() {
        return Err(DbgFlowError::Backend(
            "refusing to attach to the current dbgflow process".to_string(),
        ));
    }
    Ok(DebugTarget::Attach { pid })
}

fn validate_launch_target(executable: &Path, args: Vec<String>) -> Result<DebugTarget> {
    if !launch_enabled() {
        return Err(DbgFlowError::Backend(
            "launch targets are disabled; set DBGFLOW_ENABLE_LAUNCH=1 to enable controlled process launch".to_string(),
        ));
    }

    let executable = executable
        .canonicalize()
        .map_err(|error| DbgFlowError::Backend(format!("invalid launch executable: {error}")))?;
    if !executable.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "launch executable is not a file: {}",
            executable.display()
        )));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(DbgFlowError::Backend(
            "launch arguments must not contain NUL bytes".to_string(),
        ));
    }

    Ok(DebugTarget::Launch { executable, args })
}

fn launch_enabled() -> bool {
    std::env::var("DBGFLOW_ENABLE_LAUNCH")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
