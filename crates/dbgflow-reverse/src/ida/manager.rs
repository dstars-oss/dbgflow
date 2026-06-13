use super::install::{resolve_ida_install, IdaRuntimeConfig};
use super::model::{FunctionInfo, ReverseSession, ReverseSessionState, SegmentInfo};
use super::target::{validate_ida_target, IdaTarget};
use super::worker::{
    OpenIdaDatabase, ProcessReverseWorkerLauncher, ReverseWorker, ReverseWorkerLauncher,
};
use dbgflow_common::artifacts::{ArtifactManager, ArtifactRef, ReverseSessionArtifactEvent};
use dbgflow_common::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use dbgflow_common::time::now_unix_ms;
use dbgflow_common::{DbgFlowError, Result, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

const BACKEND_NAME: &str = "ida-dynamic";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIdaSession {
    pub target: IdaTarget,
    #[serde(default = "default_run_auto_analysis")]
    pub run_auto_analysis: bool,
    pub startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSegmentsResult {
    pub session_id: SessionId,
    pub segments: Vec<SegmentInfo>,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFunctionsResult {
    pub session_id: SessionId,
    pub functions: Vec<FunctionInfo>,
    pub artifact: ArtifactRef,
}

#[derive(Clone)]
pub struct IdaSessionManager {
    worker_launcher: Arc<dyn ReverseWorkerLauncher>,
    workers: Arc<Mutex<HashMap<SessionId, Arc<dyn ReverseWorker>>>>,
    sessions: Arc<Mutex<HashMap<SessionId, ReverseSession>>>,
    operation_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    artifacts: ArtifactManager,
    runtime: IdaRuntimeConfig,
    logger: Arc<dyn LogSink>,
}

impl IdaSessionManager {
    pub fn new(artifact_root: impl Into<PathBuf>, runtime: IdaRuntimeConfig) -> Self {
        Self::with_worker_launcher_runtime_and_logger(
            Arc::new(ProcessReverseWorkerLauncher::new()),
            artifact_root,
            runtime,
            noop_logger(),
        )
    }

    pub fn with_logger(
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self::with_worker_launcher_runtime_and_logger(
            Arc::new(ProcessReverseWorkerLauncher::new()),
            artifact_root,
            runtime,
            logger,
        )
    }

    pub fn with_worker_launcher_runtime_and_logger(
        worker_launcher: Arc<dyn ReverseWorkerLauncher>,
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self {
            worker_launcher,
            workers: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
            artifacts: ArtifactManager::new(artifact_root),
            runtime,
            logger,
        }
    }

    pub fn create_session(&self, mut request: CreateIdaSession) -> Result<ReverseSession> {
        let requested_target = request.target.clone();
        request.target = match validate_ida_target(request.target) {
            Ok(target) => target,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "reverse_ida", "create_session_rejected")
                        .operation("ida.create_session")
                        .field("target", requested_target)
                        .error(error.to_string()),
                );
                return Err(error);
            }
        };

        if let Some(existing) = self.reusable_session_for_target(&request.target)? {
            self.log(
                LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_reused")
                    .session_id(existing.id)
                    .operation("ida.create_session")
                    .field("state", format!("{:?}", existing.state))
                    .field("target", &existing.target),
            );
            return Ok(existing);
        }

        let install = resolve_ida_install(&self.runtime)?;
        let now = now_unix_ms();
        let mut session = ReverseSession {
            id: SessionId::new(),
            backend: BACKEND_NAME.to_string(),
            target: request.target.clone(),
            state: ReverseSessionState::Starting,
            ida: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            warnings: Vec::new(),
            artifacts: Vec::new(),
            error: None,
        };
        let startup_timeout = request
            .startup_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT);

        self.initialize_session_audit(&mut session, &request, &install.install_dir)?;
        self.insert_session(session.clone())?;

        let operation_lock = self.operation_lock(session.id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;

        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_started")
                .session_id(session.id)
                .operation("ida.create_session")
                .field("target", &session.target)
                .field("install_dir", install.install_dir.display().to_string())
                .field("run_auto_analysis", request.run_auto_analysis)
                .field("startup_timeout_ms", startup_timeout.as_millis() as u64),
        );

        let started = Instant::now();
        let worker = match self.worker_launcher.spawn(
            session.id,
            install.clone(),
            self.artifacts.reverse_session_worker_log_path(session.id),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                return self.finish_create_failed(session.id, started, error);
            }
        };
        if let Err(error) = self.insert_worker(session.id, worker.clone()) {
            let _ = worker.kill("insert_worker_failed");
            return self.finish_create_failed(session.id, started, error);
        }

        let open_request = OpenIdaDatabase {
            install_dir: install.install_dir,
            target: request.target,
            run_auto_analysis: request.run_auto_analysis,
        };
        let open_result =
            self.open_database_with_timeout(worker.clone(), open_request, startup_timeout);
        match open_result {
            Ok(result) => {
                let session = self.finish_create_ready(session.id, started, result)?;
                self.spawn_worker_monitor(session.id, worker);
                Ok(session)
            }
            Err(error) => {
                let _ = self.remove_worker(session.id);
                let _ = worker.kill("create_session_failed");
                self.finish_create_failed(session.id, started, error)
            }
        }
    }

    pub fn get_session(&self, session_id: SessionId) -> Result<ReverseSession> {
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    pub fn list_sessions(&self) -> Result<Vec<ReverseSession>> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.created_at_unix_ms);
        Ok(sessions)
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<ReverseSession> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;

        let previous_state = self.update_session_state(
            session_id,
            ReverseSessionState::Closing,
            "ida.close_session",
            None,
        )?;
        if previous_state == ReverseSessionState::Closed {
            return Err(DbgFlowError::SessionClosed(session_id));
        }

        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "close_session_started")
                .session_id(session_id)
                .operation("ida.close_session")
                .field("previous_state", format!("{:?}", previous_state)),
        );

        let started = Instant::now();
        let close_result = self
            .remove_worker(session_id)
            .map(|worker| worker.close())
            .unwrap_or(Ok(()));
        match close_result {
            Ok(()) => {
                self.update_session_state(
                    session_id,
                    ReverseSessionState::Closed,
                    "ida.close_session",
                    None,
                )?;
                self.log(
                    LogEvent::new(LogLevel::Info, "reverse_ida", "close_session_finished")
                        .session_id(session_id)
                        .operation("ida.close_session")
                        .duration_ms(started.elapsed().as_millis()),
                );
                let session = self.get_session(session_id)?;
                Ok(session)
            }
            Err(error) => {
                let error_text = error.to_string();
                self.update_session_state(
                    session_id,
                    ReverseSessionState::Error,
                    "ida.close_session",
                    Some(error_text.clone()),
                )?;
                self.log(
                    LogEvent::new(LogLevel::Error, "reverse_ida", "close_session_failed")
                        .session_id(session_id)
                        .operation("ida.close_session")
                        .duration_ms(started.elapsed().as_millis())
                        .error(error_text),
                );
                let session = self.get_session(session_id)?;
                Ok(session)
            }
        }
    }

    pub fn list_segments(&self, session_id: SessionId) -> Result<ListSegmentsResult> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        self.ensure_ready(session_id, "ida.list_segments")?;
        let worker = self.worker(session_id)?;
        let segments = match worker.list_segments() {
            Ok(segments) => segments,
            Err(error) => {
                if worker.has_exited().unwrap_or(false) {
                    self.mark_worker_unavailable(session_id, error.to_string());
                }
                return Err(error);
            }
        };
        let artifact = self.artifacts.write_reverse_session_output(
            session_id,
            "segments.json",
            &json!(segments),
        )?;
        self.append_artifact_to_session(session_id, artifact.clone())?;
        self.record_event(
            session_id,
            "list_segments_finished",
            None,
            None,
            Some("ida.list_segments"),
            Some(artifact.path.clone()),
            None,
            fields([("count", json!(segments.len()))]),
        );
        Ok(ListSegmentsResult {
            session_id,
            segments,
            artifact,
        })
    }

    pub fn list_functions(&self, session_id: SessionId) -> Result<ListFunctionsResult> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        self.ensure_ready(session_id, "ida.list_functions")?;
        let worker = self.worker(session_id)?;
        let functions = match worker.list_functions() {
            Ok(functions) => functions,
            Err(error) => {
                if worker.has_exited().unwrap_or(false) {
                    self.mark_worker_unavailable(session_id, error.to_string());
                }
                return Err(error);
            }
        };
        let artifact = self.artifacts.write_reverse_session_output(
            session_id,
            "functions.json",
            &json!(functions),
        )?;
        self.append_artifact_to_session(session_id, artifact.clone())?;
        self.record_event(
            session_id,
            "list_functions_finished",
            None,
            None,
            Some("ida.list_functions"),
            Some(artifact.path.clone()),
            None,
            fields([("count", json!(functions.len()))]),
        );
        Ok(ListFunctionsResult {
            session_id,
            functions,
            artifact,
        })
    }

    fn reusable_session_for_target(&self, target: &IdaTarget) -> Result<Option<ReverseSession>> {
        Ok(self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .values()
            .find(|session| session.target == *target && session.state.is_reusable())
            .cloned())
    }

    fn initialize_session_audit(
        &self,
        session: &mut ReverseSession,
        request: &CreateIdaSession,
        install_dir: &std::path::Path,
    ) -> Result<()> {
        self.artifacts
            .initialize_reverse_session_artifacts(session.id)?;
        let request_artifact = self.artifacts.write_reverse_session_request(
            session.id,
            &json!({
                "target": request.target,
                "run_auto_analysis": request.run_auto_analysis,
                "startup_timeout_ms": request.startup_timeout_ms,
                "ida": {
                    "install_dir": install_dir
                }
            }),
        )?;
        session.artifacts.push(request_artifact);
        let metadata_artifact = self
            .artifacts
            .write_reverse_session_metadata(session.id, &json!(session))?;
        session.artifacts.push(metadata_artifact);
        self.record_event(
            session.id,
            "create_session_started",
            None,
            Some(ReverseSessionState::Starting),
            Some("ida.create_session"),
            None,
            None,
            fields([
                ("target", json!(session.target)),
                ("run_auto_analysis", json!(request.run_auto_analysis)),
            ]),
        );
        Ok(())
    }

    fn insert_session(&self, session: ReverseSession) -> Result<()> {
        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session lock map poisoned".to_string()))?
            .insert(session.id, Arc::new(Mutex::new(())));
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .insert(session.id, session);
        Ok(())
    }

    fn insert_worker(&self, session_id: SessionId, worker: Arc<dyn ReverseWorker>) -> Result<()> {
        self.workers
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA worker registry lock poisoned".to_string()))?
            .insert(session_id, worker);
        Ok(())
    }

    fn remove_worker(&self, session_id: SessionId) -> Option<Arc<dyn ReverseWorker>> {
        self.workers
            .lock()
            .ok()
            .and_then(|mut workers| workers.remove(&session_id))
    }

    fn worker(&self, session_id: SessionId) -> Result<Arc<dyn ReverseWorker>> {
        self.workers
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA worker registry lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                DbgFlowError::Backend("IDA session worker is not initialized".to_string())
            })
    }

    fn operation_lock(&self, session_id: SessionId) -> Result<Arc<Mutex<()>>> {
        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session lock map poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    fn open_database_with_timeout(
        &self,
        worker: Arc<dyn ReverseWorker>,
        request: OpenIdaDatabase,
        timeout: Duration,
    ) -> Result<super::worker::OpenIdaDatabaseResult> {
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(worker.open_database(request));
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(DbgFlowError::Backend(format!(
                "IDA open_database timed out after {} ms",
                timeout.as_millis()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DbgFlowError::Backend(
                "IDA open_database thread exited without a response".to_string(),
            )),
        }
    }

    fn finish_create_ready(
        &self,
        session_id: SessionId,
        started: Instant,
        result: super::worker::OpenIdaDatabaseResult,
    ) -> Result<ReverseSession> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        let previous_state = session.state.clone();
        session.state = ReverseSessionState::Ready;
        session.ida = Some(result.ida);
        session.warnings = result.warnings;
        session.updated_at_unix_ms = now_unix_ms();
        session.error = None;
        let updated = session.clone();
        drop(sessions);

        self.write_session_metadata(&updated)?;
        self.record_event(
            session_id,
            "create_session_finished",
            Some(previous_state),
            Some(ReverseSessionState::Ready),
            Some("ida.create_session"),
            None,
            None,
            fields([("duration_ms", json!(started.elapsed().as_millis() as u64))]),
        );
        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_finished")
                .session_id(session_id)
                .operation("ida.create_session")
                .duration_ms(started.elapsed().as_millis())
                .field("state", "Ready"),
        );
        Ok(updated)
    }

    fn finish_create_failed(
        &self,
        session_id: SessionId,
        started: Instant,
        error: DbgFlowError,
    ) -> Result<ReverseSession> {
        let error_text = error.to_string();
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        let previous_state = session.state.clone();
        session.state = ReverseSessionState::Error;
        session.updated_at_unix_ms = now_unix_ms();
        session.error = Some(error_text.clone());
        let updated = session.clone();
        drop(sessions);

        let _ = self.write_session_metadata(&updated);
        self.record_event(
            session_id,
            "create_session_failed",
            Some(previous_state),
            Some(ReverseSessionState::Error),
            Some("ida.create_session"),
            None,
            Some(error_text.clone()),
            fields([("duration_ms", json!(started.elapsed().as_millis() as u64))]),
        );
        self.log(
            LogEvent::new(LogLevel::Error, "reverse_ida", "create_session_failed")
                .session_id(session_id)
                .operation("ida.create_session")
                .duration_ms(started.elapsed().as_millis())
                .error(error_text),
        );
        Err(error)
    }

    fn ensure_ready(&self, session_id: SessionId, operation: &str) -> Result<ReverseSession> {
        let session = self.get_session(session_id)?;
        match session.state {
            ReverseSessionState::Ready => Ok(session),
            ReverseSessionState::Closed => Err(DbgFlowError::SessionClosed(session_id)),
            state => {
                let error = DbgFlowError::Backend(format!("IDA session is not ready: {state:?}"));
                self.log(
                    LogEvent::new(LogLevel::Warn, "reverse_ida", "session_not_ready")
                        .session_id(session_id)
                        .operation(operation)
                        .field("state", format!("{state:?}"))
                        .error(error.to_string()),
                );
                Err(error)
            }
        }
    }

    fn update_session_state(
        &self,
        session_id: SessionId,
        new_state: ReverseSessionState,
        operation: &str,
        error: Option<String>,
    ) -> Result<ReverseSessionState> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        let previous_state = session.state.clone();
        if previous_state == ReverseSessionState::Closed
            && new_state == ReverseSessionState::Closing
        {
            return Ok(previous_state);
        }
        session.state = new_state.clone();
        session.updated_at_unix_ms = now_unix_ms();
        session.error = error.clone();
        let updated = session.clone();
        drop(sessions);

        self.write_session_metadata(&updated)?;
        self.record_event(
            session_id,
            "session_state_changed",
            Some(previous_state.clone()),
            Some(new_state),
            Some(operation),
            None,
            error,
            Map::new(),
        );
        Ok(previous_state)
    }

    fn append_artifact_to_session(
        &self,
        session_id: SessionId,
        artifact: ArtifactRef,
    ) -> Result<()> {
        let updated = {
            let mut sessions = self.sessions.lock().map_err(|_| {
                DbgFlowError::Backend("IDA session manager lock poisoned".to_string())
            })?;
            let session = sessions
                .get_mut(&session_id)
                .ok_or(DbgFlowError::SessionNotFound(session_id))?;
            session.artifacts.push(artifact);
            session.updated_at_unix_ms = now_unix_ms();
            session.clone()
        };
        self.write_session_metadata(&updated)
    }

    fn write_session_metadata(&self, session: &ReverseSession) -> Result<()> {
        let artifact = self
            .artifacts
            .write_reverse_session_metadata(session.id, &json!(session))?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        if let Some(session) = sessions.get_mut(&session.id) {
            if !session
                .artifacts
                .iter()
                .any(|existing| existing.path == artifact.path)
            {
                session.artifacts.push(artifact);
            }
        }
        Ok(())
    }

    fn spawn_worker_monitor(&self, session_id: SessionId, worker: Arc<dyn ReverseWorker>) {
        let sessions = Arc::downgrade(&self.sessions);
        let workers = Arc::downgrade(&self.workers);
        let artifacts = self.artifacts.clone();
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
                        &artifacts,
                        &logger,
                    );
                    return;
                }
            };
            if exited {
                mark_worker_unavailable(
                    session_id,
                    "IDA reverse worker exited unexpectedly".to_string(),
                    &sessions,
                    &workers,
                    &artifacts,
                    &logger,
                );
                return;
            }
        });
    }

    fn mark_worker_unavailable(&self, session_id: SessionId, error: String) {
        mark_worker_unavailable(
            session_id,
            error,
            &Arc::downgrade(&self.sessions),
            &Arc::downgrade(&self.workers),
            &self.artifacts,
            &self.logger,
        );
    }

    fn record_event(
        &self,
        session_id: SessionId,
        event: &str,
        previous_state: Option<ReverseSessionState>,
        new_state: Option<ReverseSessionState>,
        operation: Option<&str>,
        artifact_path: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        let event = ReverseSessionArtifactEvent {
            timestamp_unix_ms: now_unix_ms(),
            event: event.to_string(),
            session_id: session_id.to_string(),
            previous_state: previous_state.map(|state| format!("{state:?}")),
            new_state: new_state.map(|state| format!("{state:?}")),
            operation: operation.map(ToString::to_string),
            artifact_path,
            error,
            fields,
        };
        if let Err(error) = self
            .artifacts
            .append_reverse_session_event(session_id, &event)
        {
            self.log(
                LogEvent::new(LogLevel::Warn, "reverse_ida", "artifact_event_failed")
                    .session_id(session_id)
                    .field("event", event.event)
                    .error(error.to_string()),
            );
        }
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }
}

impl Default for IdaSessionManager {
    fn default() -> Self {
        Self::new("artifacts", IdaRuntimeConfig::default())
    }
}

fn mark_worker_unavailable(
    session_id: SessionId,
    error: String,
    sessions: &Weak<Mutex<HashMap<SessionId, ReverseSession>>>,
    workers: &Weak<Mutex<HashMap<SessionId, Arc<dyn ReverseWorker>>>>,
    artifacts: &ArtifactManager,
    logger: &Arc<dyn LogSink>,
) {
    let Some(sessions) = sessions.upgrade() else {
        return;
    };
    let mut updated = None;
    if let Ok(mut sessions) = sessions.lock() {
        if let Some(session) = sessions.get_mut(&session_id) {
            if session.state.is_terminal() || session.state == ReverseSessionState::Closing {
                return;
            }
            let previous_state = session.state.clone();
            session.state = ReverseSessionState::Error;
            session.updated_at_unix_ms = now_unix_ms();
            session.error = Some(error.clone());
            updated = Some((previous_state, session.clone()));
        }
    }
    let Some((previous_state, session)) = updated else {
        return;
    };
    if let Some(workers) = workers.upgrade() {
        if let Ok(mut workers) = workers.lock() {
            workers.remove(&session_id);
        }
    }
    let _ = artifacts.write_reverse_session_metadata(session_id, &json!(session));
    let event = ReverseSessionArtifactEvent {
        timestamp_unix_ms: now_unix_ms(),
        event: "worker_exited_unexpectedly".to_string(),
        session_id: session_id.to_string(),
        previous_state: Some(format!("{previous_state:?}")),
        new_state: Some(format!("{:?}", ReverseSessionState::Error)),
        operation: None,
        artifact_path: None,
        error: Some(error.clone()),
        fields: Map::new(),
    };
    let _ = artifacts.append_reverse_session_event(session_id, &event);
    logger.log(
        LogEvent::new(LogLevel::Error, "reverse_ida", "worker_exited_unexpectedly")
            .session_id(session_id)
            .error(error),
    );
}

fn default_run_auto_analysis() -> bool {
    true
}

fn fields<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::install::IdaInstall;
    use super::super::model::{IdaInfo, IdaVersion};
    use super::super::worker::OpenIdaDatabaseResult;
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    #[test]
    fn create_session_opens_database_and_reuses_same_target() {
        let root = test_root("reuse");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );

        let first = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary {
                    path: target.clone(),
                },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create first");
        let second = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("reuse session");

        assert_eq!(first.id, second.id);
        assert_eq!(first.state, ReverseSessionState::Ready);
        assert_eq!(launcher.spawn_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn list_segments_and_functions_write_outputs() {
        let root = test_root("list");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let manager = manager_with_mock(&root, install);
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        let segments = manager.list_segments(session.id).expect("segments");
        let functions = manager.list_functions(session.id).expect("functions");

        assert_eq!(segments.segments[0].start_ea, "0x1000");
        assert!(segments.artifact.path.ends_with("segments.json"));
        assert_eq!(functions.functions[0].flags, "0x1");
        assert!(functions.artifact.path.ends_with("functions.json"));
    }

    #[test]
    fn close_session_closes_worker_and_state() {
        let root = test_root("close");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        let closed = manager.close_session(session.id).expect("close");

        assert_eq!(closed.state, ReverseSessionState::Closed);
        assert!(launcher.last_worker().closed.load(Ordering::Relaxed));
    }

    #[test]
    fn same_session_operations_are_serialized() {
        let root = test_root("serialized");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");
        let worker = launcher.last_worker();
        let left = manager.clone();
        let right = manager.clone();

        let a = thread::spawn(move || left.list_segments(session.id));
        let b = thread::spawn(move || right.list_segments(session.id));

        a.join().expect("join a").expect("list a");
        b.join().expect("join b").expect("list b");
        assert!(!worker.concurrent_operation.load(Ordering::Relaxed));
    }

    #[test]
    fn startup_failure_marks_session_error() {
        let root = test_root("startup-failure");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::with_open_error("open failed"));
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher,
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir),
            },
            noop_logger(),
        );

        let error = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect_err("create fails");

        assert!(error.to_string().contains("open failed"));
        let sessions = manager.list_sessions().expect("list");
        assert_eq!(sessions[0].state, ReverseSessionState::Error);
    }

    #[test]
    #[ignore = "requires a licensed local IDA Professional runtime"]
    fn real_ida_create_binary_session() {
        if std::env::var("DBGFLOW_REAL_IDA_TEST").as_deref() != Ok("1") {
            return;
        }
        let ida_dir = std::env::var_os("DBGFLOW_IDA_DIR")
            .map(PathBuf::from)
            .expect("DBGFLOW_IDA_DIR must be set");
        let root = test_root("real-create");
        let manager = IdaSessionManager::new(
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(ida_dir),
            },
        );
        let target = std::env::current_exe().expect("current test exe");

        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(60_000),
            })
            .expect("create real IDA session");

        assert_eq!(session.state, ReverseSessionState::Ready);
        let _ = manager.close_session(session.id);
    }

    #[test]
    #[ignore = "requires a licensed local IDA Professional runtime"]
    fn real_ida_list_segments_functions() {
        if std::env::var("DBGFLOW_REAL_IDA_TEST").as_deref() != Ok("1") {
            return;
        }
        let ida_dir = std::env::var_os("DBGFLOW_IDA_DIR")
            .map(PathBuf::from)
            .expect("DBGFLOW_IDA_DIR must be set");
        let root = test_root("real-list");
        let manager = IdaSessionManager::new(
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(ida_dir),
            },
        );
        let target = std::env::current_exe().expect("current test exe");
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(60_000),
            })
            .expect("create real IDA session");

        let segments = manager.list_segments(session.id).expect("segments");
        let functions = manager.list_functions(session.id).expect("functions");

        assert!(!segments.segments.is_empty());
        assert!(!functions.functions.is_empty());
        let _ = manager.close_session(session.id);
    }

    fn manager_with_mock(root: &std::path::Path, install: IdaInstall) -> IdaSessionManager {
        IdaSessionManager::with_worker_launcher_runtime_and_logger(
            Arc::new(MockReverseWorkerLauncher::default()),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir),
            },
            noop_logger(),
        )
    }

    #[derive(Default)]
    struct MockReverseWorkerLauncher {
        spawn_count: AtomicU64,
        workers: Mutex<Vec<Arc<MockReverseWorker>>>,
        open_error: Mutex<Option<String>>,
    }

    impl MockReverseWorkerLauncher {
        fn with_open_error(message: &str) -> Self {
            Self {
                open_error: Mutex::new(Some(message.to_string())),
                ..Default::default()
            }
        }

        fn last_worker(&self) -> Arc<MockReverseWorker> {
            self.workers
                .lock()
                .expect("workers")
                .last()
                .expect("worker")
                .clone()
        }
    }

    impl ReverseWorkerLauncher for MockReverseWorkerLauncher {
        fn spawn(
            &self,
            _session_id: SessionId,
            _install: IdaInstall,
            _worker_log_path: PathBuf,
        ) -> Result<Arc<dyn ReverseWorker>> {
            self.spawn_count.fetch_add(1, Ordering::Relaxed);
            let worker = Arc::new(MockReverseWorker {
                open_error: self.open_error.lock().expect("open error").clone(),
                ..Default::default()
            });
            self.workers.lock().expect("workers").push(worker.clone());
            Ok(worker)
        }
    }

    #[derive(Default)]
    struct MockReverseWorker {
        closed: AtomicBool,
        killed: AtomicBool,
        in_flight: AtomicUsize,
        concurrent_operation: AtomicBool,
        open_error: Option<String>,
    }

    impl MockReverseWorker {
        fn enter(&self) -> OperationGuard<'_> {
            if self.in_flight.fetch_add(1, Ordering::SeqCst) != 0 {
                self.concurrent_operation.store(true, Ordering::SeqCst);
            }
            OperationGuard { worker: self }
        }
    }

    struct OperationGuard<'a> {
        worker: &'a MockReverseWorker,
    }

    impl Drop for OperationGuard<'_> {
        fn drop(&mut self) {
            self.worker.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl ReverseWorker for MockReverseWorker {
        fn open_database(&self, _request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult> {
            if let Some(error) = &self.open_error {
                return Err(DbgFlowError::Backend(error.clone()));
            }
            Ok(OpenIdaDatabaseResult {
                ida: IdaInfo {
                    install_dir: PathBuf::from(r"C:\FakeIDA"),
                    version: IdaVersion {
                        major: 9,
                        minor: 3,
                        build: 260327,
                    },
                },
                warnings: Vec::new(),
            })
        }

        fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
            let _guard = self.enter();
            thread::sleep(Duration::from_millis(25));
            Ok(vec![SegmentInfo {
                index: 0,
                start_ea: "0x1000".to_string(),
                end_ea: "0x2000".to_string(),
                perm: "r-x".to_string(),
                bitness: 64,
            }])
        }

        fn list_functions(&self) -> Result<Vec<FunctionInfo>> {
            Ok(vec![FunctionInfo {
                index: 0,
                start_ea: "0x1100".to_string(),
                end_ea: "0x1200".to_string(),
                flags: "0x1".to_string(),
            }])
        }

        fn has_exited(&self) -> Result<bool> {
            Ok(self.closed.load(Ordering::Relaxed) || self.killed.load(Ordering::Relaxed))
        }

        fn close(&self) -> Result<()> {
            self.closed.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn kill(&self, _reason: &str) -> Result<()> {
            self.killed.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    fn fake_binary(root: &std::path::Path) -> PathBuf {
        let path = root.join("sample.exe");
        std::fs::write(&path, b"MZ").expect("write sample");
        path
    }

    fn fake_ida_install(root: &std::path::Path) -> IdaInstall {
        let install_dir = root.join("ida");
        std::fs::create_dir_all(&install_dir).expect("create ida dir");
        for file_name in ["ida.exe", "ida.dll", "idalib.dll", "ida.hlp"] {
            std::fs::write(install_dir.join(file_name), b"").expect("write ida file");
        }
        super::super::install::validate_ida_install_dir(&install_dir).expect("validate fake ida")
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dbgflow-ida-manager-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        root
    }
}
