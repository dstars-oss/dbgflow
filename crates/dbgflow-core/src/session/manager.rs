use super::{SessionId, SessionState};
use crate::artifacts::{ArtifactManager, ArtifactRef, CommandArtifactRecord, SessionArtifactEvent};
use crate::backend::{CreateBackendSession, DebugTarget};
use crate::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use crate::policy::CommandPolicy;
use crate::session::worker::{ProcessWorkerLauncher, SessionWorker, SessionWorkerLauncher};
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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

        self.initialize_session_audit(&session)?;

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
        let (session, should_cancel, close_event) = {
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

            let previous_state = session.state;
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
            let updated = session.clone();
            let mut fields = Map::new();
            fields.insert("had_worker".to_string(), Value::Bool(has_worker));
            fields.insert("cancel_requested".to_string(), Value::Bool(should_cancel));
            let event = session_event(
                "close_session_requested",
                &updated,
                Some(previous_state),
                Some("close_session".to_string()),
                None,
                None,
                None,
                fields,
            );
            (session.clone(), should_cancel, event)
        };
        self.record_session_event_best_effort(session_id, close_event);
        self.append_transcript_best_effort(
            session_id,
            &format!(
                "[{}] close_session requested new_state={:?} cancel_requested={}\n",
                now_unix_ms(),
                session.state,
                should_cancel
            ),
        );
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
        if let Err(error) = self.command_policy.check_command(&request.command) {
            self.audit_rejected_eval(&request, &error);
            return Err(error);
        }
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
                let error_text = error.to_string();
                self.log(
                    LogEvent::new(LogLevel::Error, "session", "eval_failed")
                        .session_id(request.session_id)
                        .operation(command.clone())
                        .duration_ms(duration_ms)
                        .field("command_id", command_id.clone())
                        .error(error_text.clone()),
                );
                let operation_finished = self.finish_operation(
                    request.session_id,
                    SessionState::Error,
                    OperationStatus::Failed,
                    None,
                    Some(error_text.clone()),
                    None,
                    duration_ms,
                )?;
                if operation_finished {
                    self.write_command_record_best_effort(
                        request.session_id,
                        &CommandArtifactRecord {
                            command_id: command_id.clone(),
                            command: command.clone(),
                            status: "Failed".to_string(),
                            output_path: None,
                            started_at_unix_ms,
                            duration_ms: Some(duration_ms),
                            output_bytes: None,
                            warnings: Vec::new(),
                            error: Some(error_text),
                            backend_session_id: session.backend_session_id.clone(),
                        },
                    );
                }
                return Err(error);
            }
        };

        let output_path = self
            .artifacts
            .command_output_path(request.session_id, &command_id);
        let artifact = match self.artifacts.write_eval_output(
            request.session_id,
            &command_id,
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
                let _ = self.finish_operation(
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
        let operation_finished = self.finish_operation(
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
        if operation_finished {
            self.write_command_record_best_effort(
                request.session_id,
                &CommandArtifactRecord {
                    command_id: command_id.clone(),
                    command: command.clone(),
                    status: "Finished".to_string(),
                    output_path: Some(output_path),
                    started_at_unix_ms,
                    duration_ms: Some(duration_ms),
                    output_bytes: Some(backend_result.output.len()),
                    warnings: backend_result.warnings.clone(),
                    error: None,
                    backend_session_id: session.backend_session_id.clone(),
                },
            );
            self.append_transcript_best_effort(
                request.session_id,
                &format!(
                    "\n--- command {} output: {} ---\n{}\n--- end command {} output ---\n",
                    command_id, command, backend_result.output, command_id
                ),
            );
        }

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
        let previous_state = session.state;
        let command_id = operation.command_id.clone();
        let command = operation.command.clone();
        session.state = state;
        session.updated_at_unix_ms = now_unix_ms();
        session.current_operation = Some(operation.command.clone());
        session.last_operation = Some(operation);
        session.error = None;
        let updated = session.clone();
        drop(sessions);
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("Running".to_string()));
        self.record_session_event_best_effort(
            session_id,
            session_event(
                "eval_started",
                &updated,
                Some(previous_state),
                Some(command.clone()),
                Some(command_id.clone()),
                None,
                None,
                fields,
            ),
        );
        self.append_transcript_best_effort(
            session_id,
            &format!(
                "[{}] eval started command_id={} command={}\n",
                now_unix_ms(),
                command_id,
                command
            ),
        );
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
    ) -> Result<bool> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        if session.state.is_terminal() || session.state == SessionState::Closing {
            return Ok(false);
        }
        let previous_state = session.state;
        let mut command_id = None;
        let mut command = None;
        session.state = state;
        session.updated_at_unix_ms = now_unix_ms();
        session.current_operation = None;
        session.error = error.clone();
        if let Some(operation) = session.last_operation.as_mut() {
            command_id = Some(operation.command_id.clone());
            command = Some(operation.command.clone());
            operation.status = status;
            operation.finished_at_unix_ms = Some(now_unix_ms());
            operation.duration_ms = Some(duration_ms);
            operation.artifact = artifact;
            operation.error = error;
            operation.output_bytes = output_bytes;
        }
        let updated = session.clone();
        drop(sessions);
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String(format!("{status:?}")));
        if let Some(output_bytes) = output_bytes {
            fields.insert(
                "output_bytes".to_string(),
                Value::Number(serde_json::Number::from(output_bytes as u64)),
            );
        }
        fields.insert(
            "duration_ms".to_string(),
            Value::Number(serde_json::Number::from(duration_ms as u64)),
        );
        let artifact_path = updated
            .last_operation
            .as_ref()
            .and_then(|operation| operation.artifact.as_ref())
            .map(|artifact| artifact.path.clone());
        let error_for_event = updated.error.clone();
        let event_name = match status {
            OperationStatus::Finished => "eval_finished",
            OperationStatus::Canceled => "eval_canceled",
            OperationStatus::Failed => "eval_failed",
            OperationStatus::CancelRequested => "eval_cancel_requested",
            OperationStatus::Running => "eval_running",
        };
        self.record_session_event_best_effort(
            session_id,
            session_event(
                event_name,
                &updated,
                Some(previous_state),
                command.clone(),
                command_id.clone(),
                artifact_path,
                error_for_event.clone(),
                fields,
            ),
        );
        if let (Some(command_id), Some(command)) = (command_id, command) {
            self.append_transcript_best_effort(
                session_id,
                &format!(
                    "[{}] {} command_id={} command={} duration_ms={} error={}\n",
                    now_unix_ms(),
                    event_name,
                    command_id,
                    command,
                    duration_ms,
                    error_for_event.unwrap_or_default()
                ),
            );
        }
        self.notify_session_updated(session_id);
        Ok(true)
    }

    fn notify_session_updated(&self, session_id: SessionId) {
        if let Ok(mut subscribers) = self.event_subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(session_id).is_ok());
        }
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }

    fn initialize_session_audit(&self, session: &Session) -> Result<()> {
        self.artifacts.initialize_session_artifacts(session.id)?;

        let mut fields = Map::new();
        fields.insert(
            "target".to_string(),
            serde_json::to_value(&session.target)
                .unwrap_or_else(|_| Value::String("<serialize error>".to_string())),
        );
        self.artifacts.append_event(
            session.id,
            &session_event(
                "session_created",
                session,
                None,
                Some("create_session".to_string()),
                None,
                None,
                None,
                fields,
            ),
        )?;
        self.artifacts.append_transcript(
            session.id,
            &format!(
                "[{}] session created state={:?} backend={} target={:?}\n",
                now_unix_ms(),
                session.state,
                session.backend,
                session.target
            ),
        )?;

        Ok(())
    }

    fn audit_rejected_eval(&self, request: &EvalSession, error: &DbgFlowError) {
        let Ok(session) = self.query_session(request.session_id) else {
            return;
        };
        let command_id = SessionId::new().to_string();
        let started_at_unix_ms = now_unix_ms();
        let error = error.to_string();
        self.write_command_record_best_effort(
            request.session_id,
            &CommandArtifactRecord {
                command_id: command_id.clone(),
                command: request.command.clone(),
                status: "Rejected".to_string(),
                output_path: None,
                started_at_unix_ms,
                duration_ms: Some(0),
                output_bytes: None,
                warnings: Vec::new(),
                error: Some(error.clone()),
                backend_session_id: session.backend_session_id.clone(),
            },
        );
        self.record_session_event_best_effort(
            request.session_id,
            session_event(
                "eval_rejected",
                &session,
                Some(session.state),
                Some(request.command.clone()),
                Some(command_id.clone()),
                None,
                Some(error.clone()),
                Map::new(),
            ),
        );
        self.append_transcript_best_effort(
            request.session_id,
            &format!(
                "[{}] eval rejected command_id={} command={} error={}\n",
                now_unix_ms(),
                command_id,
                request.command,
                error
            ),
        );
    }

    fn write_command_record_best_effort(
        &self,
        session_id: SessionId,
        record: &CommandArtifactRecord,
    ) {
        if let Err(error) = self.artifacts.append_command_record(session_id, record) {
            self.log(
                LogEvent::new(LogLevel::Warn, "session", "command_artifact_record_failed")
                    .session_id(session_id)
                    .field("command_id", record.command_id.clone())
                    .error(error.to_string()),
            );
        }
    }

    fn record_session_event_best_effort(&self, session_id: SessionId, event: SessionArtifactEvent) {
        if let Err(error) = self.artifacts.append_event(session_id, &event) {
            self.log(
                LogEvent::new(LogLevel::Warn, "session", "session_artifact_event_failed")
                    .session_id(session_id)
                    .field("event", event.event)
                    .error(error.to_string()),
            );
        }
    }

    fn append_transcript_best_effort(&self, session_id: SessionId, text: &str) {
        if let Err(error) = self.artifacts.append_transcript(session_id, text) {
            self.log(
                LogEvent::new(
                    LogLevel::Warn,
                    "session",
                    "session_transcript_append_failed",
                )
                .session_id(session_id)
                .error(error.to_string()),
            );
        }
    }

    fn finish_worker_close(&self, session_id: SessionId, close_result: Result<()>) {
        let _ = self.remove_worker(session_id);
        let mut audit_event = None;
        let mut transcript = None;
        let mut finalized_session = None;
        let mut finalized_operation = None;
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&session_id) {
                let previous_state = session.state;
                match close_result {
                    Ok(()) => {
                        self.log(
                            LogEvent::new(LogLevel::Info, "session", "worker_close_finished")
                                .session_id(session_id),
                        );
                        finalized_operation = finalize_canceled_operation(
                            session,
                            OperationStatus::Canceled,
                            "operation canceled by close_session".to_string(),
                        );
                        session.state = SessionState::Closed;
                        session.error = None;
                        let updated = session.clone();
                        let mut fields = Map::new();
                        fields.insert("status".to_string(), Value::String("Closed".to_string()));
                        audit_event = Some(session_event(
                            "worker_close_finished",
                            &updated,
                            Some(previous_state),
                            Some("close_session".to_string()),
                            None,
                            None,
                            None,
                            fields,
                        ));
                        transcript = Some(format!(
                            "[{}] worker close finished new_state={:?}\n",
                            now_unix_ms(),
                            updated.state
                        ));
                        finalized_session = Some(updated);
                    }
                    Err(error) => {
                        let error = error.to_string();
                        self.log(
                            LogEvent::new(LogLevel::Error, "session", "worker_close_failed")
                                .session_id(session_id)
                                .error(error.clone()),
                        );
                        finalized_operation = finalize_canceled_operation(
                            session,
                            OperationStatus::Failed,
                            error.clone(),
                        );
                        session.state = SessionState::Error;
                        session.error = Some(error.clone());
                        let updated = session.clone();
                        let mut fields = Map::new();
                        fields.insert("status".to_string(), Value::String("Error".to_string()));
                        audit_event = Some(session_event(
                            "worker_close_failed",
                            &updated,
                            Some(previous_state),
                            Some("close_session".to_string()),
                            None,
                            None,
                            Some(error.clone()),
                            fields,
                        ));
                        transcript = Some(format!(
                            "[{}] worker close failed new_state={:?} error={}\n",
                            now_unix_ms(),
                            updated.state,
                            error
                        ));
                        finalized_session = Some(updated);
                    }
                }
                session.updated_at_unix_ms = now_unix_ms();
                session.current_operation = None;
            }
        }
        if let Some(event) = audit_event {
            self.record_session_event_best_effort(session_id, event);
        }
        if let Some(transcript) = transcript {
            self.append_transcript_best_effort(session_id, &transcript);
        }
        if let (Some(session), Some(operation)) = (finalized_session, finalized_operation) {
            self.write_command_record_best_effort(
                session_id,
                &command_record_from_operation(&operation, session.backend_session_id.clone()),
            );
            self.record_session_event_best_effort(
                session_id,
                operation_artifact_event(
                    &session,
                    Some(SessionState::Closing),
                    &operation,
                    match operation.status {
                        OperationStatus::Canceled => "eval_canceled",
                        OperationStatus::Failed => "eval_failed",
                        OperationStatus::Finished => "eval_finished",
                        OperationStatus::CancelRequested => "eval_cancel_requested",
                        OperationStatus::Running => "eval_running",
                    },
                ),
            );
            self.append_transcript_best_effort(
                session_id,
                &format!(
                    "[{}] {} command_id={} command={} error={}\n",
                    now_unix_ms(),
                    match operation.status {
                        OperationStatus::Canceled => "eval_canceled",
                        OperationStatus::Failed => "eval_failed",
                        OperationStatus::Finished => "eval_finished",
                        OperationStatus::CancelRequested => "eval_cancel_requested",
                        OperationStatus::Running => "eval_running",
                    },
                    operation.command_id,
                    operation.command,
                    operation.error.unwrap_or_default()
                ),
            );
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
                            let previous_state = session.state;
                            session.backend = worker_session.backend;
                            session.backend_session_id =
                                Some(worker_session.backend_session_id.clone());
                            session.state = state_for_target(&session.target);
                            session.updated_at_unix_ms = now_unix_ms();
                            session.warnings = worker_session.warnings;
                            session.current_operation = None;
                            session.error = None;
                            let updated = session.clone();
                            let mut fields = Map::new();
                            fields.insert(
                                "duration_ms".to_string(),
                                Value::Number(serde_json::Number::from(duration_ms as u64)),
                            );
                            let audit_event = session_event(
                                "worker_startup_finished",
                                &updated,
                                Some(previous_state),
                                Some("create_session".to_string()),
                                None,
                                None,
                                None,
                                fields,
                            );
                            let transcript = format!(
                                "[{}] worker startup finished backend={} backend_session_id={} new_state={:?} duration_ms={}\n",
                                now_unix_ms(),
                                updated.backend,
                                updated
                                    .backend_session_id
                                    .as_deref()
                                    .unwrap_or(""),
                                updated.state,
                                duration_ms
                            );
                            drop(sessions);
                            self.record_session_event_best_effort(session_id, audit_event);
                            self.append_transcript_best_effort(session_id, &transcript);
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
                let error = error.to_string();
                self.log(
                    LogEvent::new(LogLevel::Error, "session", "worker_startup_failed")
                        .session_id(session_id)
                        .operation("create_session")
                        .duration_ms(duration_ms)
                        .error(error.clone()),
                );
                if let Ok(mut sessions) = self.sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id) {
                        if session.state == SessionState::Closing {
                            let previous_state = session.state;
                            session.state = SessionState::Closed;
                            session.updated_at_unix_ms = now_unix_ms();
                            session.current_operation = None;
                            session.error = None;
                            let updated = session.clone();
                            let mut fields = Map::new();
                            fields.insert(
                                "duration_ms".to_string(),
                                Value::Number(serde_json::Number::from(duration_ms as u64)),
                            );
                            let audit_event = session_event(
                                "worker_startup_failed_after_close",
                                &updated,
                                Some(previous_state),
                                Some("create_session".to_string()),
                                None,
                                None,
                                Some(error.clone()),
                                fields,
                            );
                            let transcript = format!(
                                "[{}] worker startup failed after close new_state={:?} error={} duration_ms={}\n",
                                now_unix_ms(),
                                updated.state,
                                error,
                                duration_ms
                            );
                            drop(sessions);
                            self.record_session_event_best_effort(session_id, audit_event);
                            self.append_transcript_best_effort(session_id, &transcript);
                            self.notify_session_updated(session_id);
                        } else if !session.state.is_terminal() {
                            let previous_state = session.state;
                            session.state = SessionState::Error;
                            session.updated_at_unix_ms = now_unix_ms();
                            session.current_operation = None;
                            session.error = Some(error.clone());
                            let updated = session.clone();
                            let mut fields = Map::new();
                            fields.insert(
                                "duration_ms".to_string(),
                                Value::Number(serde_json::Number::from(duration_ms as u64)),
                            );
                            let audit_event = session_event(
                                "worker_startup_failed",
                                &updated,
                                Some(previous_state),
                                Some("create_session".to_string()),
                                None,
                                None,
                                Some(error.clone()),
                                fields,
                            );
                            let transcript = format!(
                                "[{}] worker startup failed new_state={:?} error={} duration_ms={}\n",
                                now_unix_ms(),
                                updated.state,
                                error,
                                duration_ms
                            );
                            drop(sessions);
                            self.record_session_event_best_effort(session_id, audit_event);
                            self.append_transcript_best_effort(session_id, &transcript);
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
        let artifacts = self.artifacts.clone();
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
                        &artifacts,
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
                &artifacts,
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

fn finalize_canceled_operation(
    session: &mut Session,
    status: OperationStatus,
    error: String,
) -> Option<OperationSummary> {
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
            return Some(operation.clone());
        }
    }
    None
}

fn mark_worker_unavailable(
    session_id: SessionId,
    error: String,
    sessions: &Weak<Mutex<HashMap<SessionId, Session>>>,
    workers: &Weak<WorkerRegistry>,
    event_subscribers: &Weak<Mutex<Vec<mpsc::Sender<SessionId>>>>,
    logger: &Arc<dyn LogSink>,
    artifacts: &ArtifactManager,
) {
    let Some(sessions) = sessions.upgrade() else {
        return;
    };
    let mut should_notify = false;
    let mut audit_event = None;
    let mut transcript = None;
    if let Ok(mut sessions) = sessions.lock() {
        if let Some(session) = sessions.get_mut(&session_id) {
            if session.state == SessionState::Closing || session.state.is_terminal() {
                return;
            }
            let previous_state = session.state;
            finalize_canceled_operation(session, OperationStatus::Failed, error.clone());
            session.state = SessionState::Error;
            session.updated_at_unix_ms = now_unix_ms();
            session.current_operation = None;
            session.error = Some(error.clone());
            let updated = session.clone();
            audit_event = Some(session_event(
                "worker_exited_unexpectedly",
                &updated,
                Some(previous_state),
                None,
                None,
                None,
                Some(error.clone()),
                Map::new(),
            ));
            transcript = Some(format!(
                "[{}] worker unavailable new_state={:?} error={}\n",
                now_unix_ms(),
                updated.state,
                error
            ));
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
        if let Some(event) = audit_event {
            if let Err(error) = artifacts.append_event(session_id, &event) {
                logger.log(
                    LogEvent::new(LogLevel::Warn, "session", "session_artifact_event_failed")
                        .session_id(session_id)
                        .error(error.to_string()),
                );
            }
        }
        if let Some(transcript) = transcript {
            if let Err(error) = artifacts.append_transcript(session_id, &transcript) {
                logger.log(
                    LogEvent::new(
                        LogLevel::Warn,
                        "session",
                        "session_transcript_append_failed",
                    )
                    .session_id(session_id)
                    .error(error.to_string()),
                );
            }
        }
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

fn session_event(
    event: impl Into<String>,
    session: &Session,
    previous_state: Option<SessionState>,
    operation: Option<String>,
    command_id: Option<String>,
    artifact_path: Option<PathBuf>,
    error: Option<String>,
    fields: Map<String, Value>,
) -> SessionArtifactEvent {
    SessionArtifactEvent {
        timestamp_unix_ms: now_unix_ms(),
        event: event.into(),
        session_id: session.id.to_string(),
        previous_state: previous_state.map(|state| format!("{state:?}")),
        new_state: Some(format!("{:?}", session.state)),
        backend: Some(session.backend.clone()),
        backend_session_id: session.backend_session_id.clone(),
        operation,
        command_id,
        artifact_path,
        error,
        fields,
    }
}

fn operation_artifact_event(
    session: &Session,
    previous_state: Option<SessionState>,
    operation: &OperationSummary,
    event: &'static str,
) -> SessionArtifactEvent {
    let mut fields = Map::new();
    fields.insert(
        "status".to_string(),
        Value::String(operation_status_label(operation.status).to_string()),
    );
    if let Some(duration_ms) = operation.duration_ms {
        fields.insert(
            "duration_ms".to_string(),
            Value::Number(serde_json::Number::from(duration_ms as u64)),
        );
    }
    if let Some(output_bytes) = operation.output_bytes {
        fields.insert(
            "output_bytes".to_string(),
            Value::Number(serde_json::Number::from(output_bytes as u64)),
        );
    }
    session_event(
        event,
        session,
        previous_state,
        Some(operation.command.clone()),
        Some(operation.command_id.clone()),
        operation
            .artifact
            .as_ref()
            .map(|artifact| artifact.path.clone()),
        operation.error.clone(),
        fields,
    )
}

fn command_record_from_operation(
    operation: &OperationSummary,
    backend_session_id: Option<String>,
) -> CommandArtifactRecord {
    CommandArtifactRecord {
        command_id: operation.command_id.clone(),
        command: operation.command.clone(),
        status: operation_status_label(operation.status).to_string(),
        output_path: operation
            .artifact
            .as_ref()
            .map(|artifact| artifact.path.clone()),
        started_at_unix_ms: operation.started_at_unix_ms,
        duration_ms: operation.duration_ms,
        output_bytes: operation.output_bytes,
        warnings: Vec::new(),
        error: operation.error.clone(),
        backend_session_id,
    }
}

fn operation_status_label(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Running => "Running",
        OperationStatus::CancelRequested => "CancelRequested",
        OperationStatus::Canceled => "Canceled",
        OperationStatus::Finished => "Finished",
        OperationStatus::Failed => "Failed",
    }
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
