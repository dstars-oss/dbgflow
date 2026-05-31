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
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_EXECUTE_TIMEOUT_MS: u64 = 120_000;
const OUTPUT_PREVIEW_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub target: DebugTarget,
}

impl Default for CreateSession {
    fn default() -> Self {
        Self {
            target: DebugTarget::Mock,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub backend: String,
    pub backend_session_id: String,
    pub target: DebugTarget,
    pub state: SessionState,
    pub created_at_unix_ms: u128,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
pub struct SessionManager {
    backends: HashMap<String, Arc<dyn DebugBackend>>,
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    operation_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    artifacts: ArtifactManager,
    command_policy: CommandPolicy,
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

        let backend_name = select_backend_for_target(&request.target);
        let backend = self
            .backends
            .get(&backend_name)
            .ok_or_else(|| DbgFlowError::BackendNotFound(backend_name.clone()))?;

        let backend_session = backend.create_session(CreateBackendSession {
            target: request.target.clone(),
        })?;

        let state = state_for_target(&request.target);
        let session = Session {
            id: SessionId::new(),
            backend: backend_name,
            backend_session_id: backend_session.id,
            target: request.target,
            state,
            created_at_unix_ms: now_unix_ms(),
            warnings: backend_session.warnings,
        };

        sessions.insert(session.id, session.clone());
        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("session lock map poisoned".to_string()))?
            .insert(session.id, Arc::new(Mutex::new(())));

        Ok(session)
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
        let _operation_guard = operation_lock
            .lock()
            .map_err(|_| DbgFlowError::Backend("session operation lock poisoned".to_string()))?;

        let (backend_name, backend_session_id) = {
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

            session.state = SessionState::Closing;
            (session.backend.clone(), session.backend_session_id.clone())
        };

        let backend = self
            .backends
            .get(&backend_name)
            .ok_or_else(|| DbgFlowError::BackendNotFound(backend_name.clone()))?;

        let close_result = backend.close_session(&backend_session_id);

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;

        match close_result {
            Ok(()) => {
                session.state = SessionState::Closed;
                Ok(session.clone())
            }
            Err(error) => {
                session.state = SessionState::Error;
                Err(error)
            }
        }
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
        if is_run_control {
            self.set_session_state(request.session_id, SessionState::Running)?;
        }

        let backend = self
            .backends
            .get(&session.backend)
            .ok_or_else(|| DbgFlowError::BackendNotFound(session.backend.clone()))?;

        let started_at_unix_ms = now_unix_ms();
        let started = Instant::now();
        let command_id = SessionId::new().to_string();
        let backend_result = backend.execute(ExecuteBackendRequest {
            backend_session_id: session.backend_session_id.clone(),
            command: request.command.clone(),
            timeout_ms: request.timeout_ms.unwrap_or(DEFAULT_EXECUTE_TIMEOUT_MS),
        });
        let duration_ms = started.elapsed().as_millis();

        let backend_result = match backend_result {
            Ok(result) => result,
            Err(error) => {
                self.set_session_state(request.session_id, SessionState::Error)?;
                return Err(error);
            }
        };
        if is_run_control {
            self.set_session_state(request.session_id, SessionState::Break)?;
        }

        let (output_preview, output_truncated) =
            truncate_for_preview(&backend_result.output, OUTPUT_PREVIEW_LIMIT);
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
                output_truncated_in_response: output_truncated,
            },
            &backend_result.output,
        )?;

        Ok(ExecuteSessionResult {
            session: self.query_session(request.session_id)?,
            output_preview,
            output_truncated,
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

    fn set_session_state(&self, session_id: SessionId, state: SessionState) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        session.state = state;
        Ok(())
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
    pub output_preview: String,
    pub output_truncated: bool,
    pub artifact: ArtifactRef,
    pub warnings: Vec<String>,
    pub duration_ms: u128,
}

fn truncate_for_preview(output: &str, limit: usize) -> (String, bool) {
    if output.len() <= limit {
        return (output.to_string(), false);
    }

    let mut end = limit;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    (output[..end].to_string(), true)
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
