use super::{SessionId, SessionState};
use crate::artifacts::{ArtifactManager, ArtifactRef, CommandArtifactRecord};
#[cfg(windows)]
use crate::backend::dbgeng::DbgEngBackend;
use crate::backend::mock::MockBackend;
use crate::backend::{CreateBackendSession, DebugBackend, DebugTarget, ExecuteBackendRequest};
use crate::policy::CommandPolicy;
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_EXECUTE_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub target: DebugTarget,
    pub startup_timeout_ms: Option<u64>,
}

impl Default for CreateSession {
    fn default() -> Self {
        Self {
            target: DebugTarget::Mock,
            startup_timeout_ms: None,
        }
    }
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
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct SessionManager {
    backends: HashMap<String, Arc<dyn DebugBackend>>,
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    operation_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    artifacts: ArtifactManager,
    command_policy: CommandPolicy,
    event_subscribers: Arc<Mutex<Vec<mpsc::Sender<SessionId>>>>,
}

impl SessionManager {
    pub fn new(backends: Vec<Arc<dyn DebugBackend>>) -> Self {
        Self::with_artifact_root(backends, "artifacts")
    }

    pub fn with_artifact_root(
        backends: Vec<Arc<dyn DebugBackend>>,
        artifact_root: impl Into<PathBuf>,
    ) -> Self {
        let backends = backends
            .into_iter()
            .map(|backend| (backend.info().name, backend))
            .collect();

        Self {
            backends,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
            artifacts: ArtifactManager::new(artifact_root),
            command_policy: CommandPolicy::default_query_policy(),
            event_subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_mock_backend() -> Self {
        Self::new(vec![Arc::new(MockBackend::new())])
    }

    pub fn with_default_backends() -> Self {
        Self::with_default_backends_at("artifacts")
    }

    pub fn with_default_backends_at(artifact_root: impl Into<PathBuf>) -> Self {
        let mut backends: Vec<Arc<dyn DebugBackend>> = vec![Arc::new(MockBackend::new())];
        #[cfg(windows)]
        {
            backends.push(Arc::new(DbgEngBackend::new()));
        }
        Self::with_artifact_root(backends, artifact_root)
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

        let backend_name = select_backend_for_target(&request.target);
        let backend = self
            .backends
            .get(&backend_name)
            .ok_or_else(|| DbgFlowError::BackendNotFound(backend_name.clone()))?
            .clone();

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;

        if let Some(existing) = sessions
            .values()
            .find(|session| session.target == request.target && !session.state.is_terminal())
            .cloned()
        {
            return Ok(existing);
        }

        let now = now_unix_ms();
        let session = Session {
            id: SessionId::new(),
            backend: backend_name,
            backend_session_id: None,
            target: request.target.clone(),
            state: SessionState::Starting,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            warnings: Vec::new(),
            current_operation: Some("create_session".to_string()),
            error: None,
        };

        sessions.insert(session.id, session.clone());
        drop(sessions);

        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("session lock map poisoned".to_string()))?
            .insert(session.id, Arc::new(Mutex::new(())));

        self.spawn_backend_startup(
            session.id,
            backend,
            request.target,
            request
                .startup_timeout_ms
                .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS),
        );
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
        let (session, backend_name, backend_session_id) = {
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

            session.state = if session.backend_session_id.is_some() {
                SessionState::Closing
            } else {
                SessionState::Closed
            };
            session.updated_at_unix_ms = now_unix_ms();
            session.current_operation = None;
            (
                session.clone(),
                session.backend.clone(),
                session.backend_session_id.clone(),
            )
        };
        self.notify_session_updated(session_id);

        if let Some(backend_session_id) = backend_session_id {
            if let Some(backend) = self.backends.get(&backend_name).cloned() {
                let manager = self.clone();
                thread::spawn(move || {
                    let _operation_guard = operation_lock.lock();
                    let close_result = backend.close_session(&backend_session_id);
                    manager.finish_backend_close(session_id, close_result);
                });
            }
        }

        Ok(session)
    }

    pub fn execute(&self, request: ExecuteSession) -> Result<ExecuteSessionResult> {
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
        let backend_session_id = session.backend_session_id.clone().ok_or_else(|| {
            DbgFlowError::Backend("session backend is not initialized".to_string())
        })?;
        if is_run_control {
            self.set_session_state(
                request.session_id,
                SessionState::Running,
                Some(request.command.clone()),
                None,
            )?;
        }

        let backend = self
            .backends
            .get(&session.backend)
            .ok_or_else(|| DbgFlowError::BackendNotFound(session.backend.clone()))?;

        let started_at_unix_ms = now_unix_ms();
        let started = Instant::now();
        let command_id = SessionId::new().to_string();
        let backend_result = backend.execute(ExecuteBackendRequest {
            backend_session_id,
            command: request.command.clone(),
            timeout_ms: request.timeout_ms.unwrap_or(DEFAULT_EXECUTE_TIMEOUT_MS),
        });
        let duration_ms = started.elapsed().as_millis();

        let backend_result = match backend_result {
            Ok(result) => result,
            Err(error) => {
                self.set_session_state(
                    request.session_id,
                    SessionState::Error,
                    None,
                    Some(error.to_string()),
                )?;
                return Err(error);
            }
        };
        if is_run_control {
            self.set_session_state(request.session_id, SessionState::Break, None, None)?;
        }

        let output_path = self
            .artifacts
            .root()
            .join("sessions")
            .join(request.session_id.to_string())
            .join("outputs")
            .join(format!("{command_id}.txt"));
        let artifact = self.artifacts.write_execute_artifacts(
            request.session_id,
            &command_id,
            &CommandArtifactRecord {
                command_id: command_id.clone(),
                command: request.command,
                output_path,
                started_at_unix_ms,
                duration_ms,
                output_bytes: backend_result.output.len(),
            },
            &backend_result.output,
        )?;

        Ok(ExecuteSessionResult {
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

    fn set_session_state(
        &self,
        session_id: SessionId,
        state: SessionState,
        current_operation: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        if session.state.is_terminal() {
            return Ok(());
        }
        if session.state == SessionState::Closing {
            return Ok(());
        }
        session.state = state;
        session.updated_at_unix_ms = now_unix_ms();
        session.current_operation = current_operation;
        session.error = error;
        self.notify_session_updated(session_id);
        Ok(())
    }

    fn notify_session_updated(&self, session_id: SessionId) {
        if let Ok(mut subscribers) = self.event_subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(session_id).is_ok());
        }
    }

    fn finish_backend_close(&self, session_id: SessionId, close_result: Result<()>) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&session_id) {
                match close_result {
                    Ok(()) => {
                        session.state = SessionState::Closed;
                        session.error = None;
                    }
                    Err(error) => {
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

    fn spawn_backend_startup(
        &self,
        session_id: SessionId,
        backend: Arc<dyn DebugBackend>,
        target: DebugTarget,
        startup_timeout_ms: u64,
    ) {
        let manager = self.clone();
        thread::spawn(move || {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            let backend_for_startup = backend.clone();
            thread::spawn(move || {
                let result = backend_for_startup.create_session(CreateBackendSession {
                    target: target.clone(),
                });
                let _ = reply_tx.send(result);
            });

            match reply_rx.recv_timeout(Duration::from_millis(startup_timeout_ms)) {
                Ok(startup_result) => manager.finish_backend_startup(session_id, startup_result),
                Err(_) => {
                    manager.finish_backend_startup(
                        session_id,
                        Err(DbgFlowError::Backend(format!(
                            "backend startup timed out after {startup_timeout_ms}ms"
                        ))),
                    );
                    thread::spawn(move || {
                        if let Ok(Ok(backend_session)) = reply_rx.recv() {
                            let _ = backend.close_session(&backend_session.id);
                        }
                    });
                }
            }
        });
    }

    fn finish_backend_startup(
        &self,
        session_id: SessionId,
        startup_result: Result<crate::backend::BackendSession>,
    ) {
        match startup_result {
            Ok(backend_session) => {
                let mut should_close_backend = false;
                if let Ok(mut sessions) = self.sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id) {
                        if session.state.is_terminal() || session.state == SessionState::Closing {
                            should_close_backend = true;
                        } else {
                            session.backend_session_id = Some(backend_session.id.clone());
                            session.state = state_for_target(&session.target);
                            session.updated_at_unix_ms = now_unix_ms();
                            session.warnings = backend_session.warnings;
                            session.current_operation = None;
                            session.error = None;
                            drop(sessions);
                            self.notify_session_updated(session_id);
                        }
                    } else {
                        should_close_backend = true;
                    }
                }

                if should_close_backend {
                    if let Some(backend_name) = self.sessions.lock().ok().and_then(|sessions| {
                        sessions
                            .get(&session_id)
                            .map(|session| session.backend.clone())
                    }) {
                        if let Some(backend) = self.backends.get(&backend_name) {
                            let _ = backend.close_session(&backend_session.id);
                        }
                    }
                }
            }
            Err(error) => {
                if let Ok(mut sessions) = self.sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id) {
                        if !session.state.is_terminal() {
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

    fn validate_target(&self, target: DebugTarget) -> Result<DebugTarget> {
        match target {
            DebugTarget::Mock => Ok(DebugTarget::Mock),
            DebugTarget::Dump { path } => {
                validate_dump_target(&path).map(|path| DebugTarget::Dump { path })
            }
            DebugTarget::Attach { pid } => validate_attach_target(pid),
            DebugTarget::Launch { executable, args } => validate_launch_target(&executable, args),
        }
    }
}

fn select_backend_for_target(target: &DebugTarget) -> String {
    match target {
        DebugTarget::Mock => "mock".to_string(),
        DebugTarget::Dump { .. } | DebugTarget::Attach { .. } | DebugTarget::Launch { .. } => {
            "dbgeng".to_string()
        }
    }
}

fn state_for_target(target: &DebugTarget) -> SessionState {
    match target {
        DebugTarget::Mock => SessionState::Ready,
        DebugTarget::Dump { .. } | DebugTarget::Attach { .. } | DebugTarget::Launch { .. } => {
            SessionState::Break
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
pub struct ExecuteSession {
    pub session_id: SessionId,
    pub command: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteSessionResult {
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
