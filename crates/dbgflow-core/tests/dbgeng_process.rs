#![cfg(windows)]

use dbgflow_core::backend::dbgeng::DbgEngBackend;
use dbgflow_core::backend::{
    CreateBackendSession, DebugBackend, DebugTarget, ExecuteBackendResult,
};
use dbgflow_core::session::worker::{SessionWorker, SessionWorkerLauncher, WorkerSession};
use dbgflow_core::session::{CreateSession, EvalSession, SessionManager, SessionState};
use dbgflow_core::Result;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const CHILD_SLEEP_ENV: &str = "DBGFLOW_PROCESS_SLEEP_CHILD";

#[test]
#[ignore = "live DbgEng process debugging is environment-sensitive; run explicitly when validating attach"]
fn dbgeng_can_attach_to_process_and_query_modules() {
    let _guard = live_debug_lock().lock().expect("live debug test lock");
    let mut child = spawn_sleep_child();
    std::thread::sleep(Duration::from_millis(500));

    let artifact_root = test_artifact_root("dbgeng-attach");
    let manager = dbgeng_manager(&artifact_root);
    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Attach { pid: child.id() },
            startup_timeout_ms: None,
        })
        .expect("attach process session");
    let session = wait_for_break(&manager, session.id);
    assert_eq!(session.state, SessionState::Break);

    let result = manager
        .eval(EvalSession {
            session_id: session.id,
            command: "lm".to_string(),
            timeout_ms: Some(120_000),
        })
        .expect("query modules after attach");

    assert!(
        !result.output.trim().is_empty(),
        "expected module output after attach"
    );

    manager
        .close_session(session.id)
        .expect("close attach session");
    cleanup_child(&mut child);
}

#[test]
#[ignore = "live DbgEng process debugging is environment-sensitive; run explicitly when validating launch"]
fn dbgeng_can_launch_process_and_continue_to_exit() {
    let _guard = live_debug_lock().lock().expect("live debug test lock");
    let ping = ping_exe();
    let artifact_root = test_artifact_root("dbgeng-launch");
    let manager = dbgeng_manager(&artifact_root);
    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Launch {
                executable: ping,
                args: vec!["127.0.0.1".to_string(), "-n".to_string(), "3".to_string()],
            },
            startup_timeout_ms: None,
        })
        .expect("launch process session");
    let session = wait_for_break(&manager, session.id);
    assert_eq!(session.state, SessionState::Break);

    let result = manager
        .eval(EvalSession {
            session_id: session.id,
            command: "g".to_string(),
            timeout_ms: Some(15_000),
        })
        .expect("continue launched process");

    assert_eq!(result.session.state, SessionState::Closed);
    assert!(result.artifact.path.is_file());
}

#[test]
fn process_child_sleep_entrypoint() {
    if std::env::var_os(CHILD_SLEEP_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn spawn_sleep_child() -> Child {
    Command::new(std::env::current_exe().expect("current test exe"))
        .arg("process_child_sleep_entrypoint")
        .arg("--exact")
        .env(CHILD_SLEEP_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sleep child")
}

fn cleanup_child(child: &mut Child) {
    if child.try_wait().expect("poll child").is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn dbgeng_manager(artifact_root: &PathBuf) -> SessionManager {
    SessionManager::with_worker_launcher(Arc::new(InProcessDbgEngWorkerLauncher), artifact_root)
}

fn ping_exe() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("ping.exe")
}

fn test_artifact_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create artifact root");
    root
}

fn live_debug_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn wait_for_break(
    manager: &SessionManager,
    session_id: dbgflow_core::session::SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + Duration::from_secs(130);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Break {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not break: {session:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct InProcessDbgEngWorkerLauncher;

impl SessionWorkerLauncher for InProcessDbgEngWorkerLauncher {
    fn spawn(
        &self,
        _session_id: dbgflow_core::session::SessionId,
        _logger: Arc<dyn dbgflow_core::logging::LogSink>,
    ) -> Result<Arc<dyn SessionWorker>> {
        Ok(Arc::new(InProcessDbgEngWorker {
            backend: DbgEngBackend::new(),
            backend_session_id: Mutex::new(None),
        }))
    }
}

struct InProcessDbgEngWorker {
    backend: DbgEngBackend,
    backend_session_id: Mutex<Option<String>>,
}

impl SessionWorker for InProcessDbgEngWorker {
    fn create_session(&self, request: CreateBackendSession) -> Result<WorkerSession> {
        let session = self.backend.create_session(request)?;
        *self.backend_session_id.lock().map_err(|_| {
            dbgflow_core::DbgFlowError::Backend("test worker lock poisoned".into())
        })? = Some(session.id.clone());
        Ok(WorkerSession {
            backend: self.backend.info().name,
            backend_session_id: session.id,
            warnings: session.warnings,
        })
    }

    fn execute(
        &self,
        command: String,
        event_sink: std::sync::Arc<dyn dbgflow_core::backend::BackendEventSink>,
    ) -> Result<ExecuteBackendResult> {
        let backend_session_id = self
            .backend_session_id
            .lock()
            .map_err(|_| dbgflow_core::DbgFlowError::Backend("test worker lock poisoned".into()))?
            .clone()
            .ok_or_else(|| {
                dbgflow_core::DbgFlowError::Backend("test worker not initialized".into())
            })?;
        self.backend.execute(
            dbgflow_core::backend::ExecuteBackendRequest {
                backend_session_id,
                command,
            },
            event_sink,
        )
    }

    fn has_exited(&self) -> Result<bool> {
        Ok(false)
    }

    fn close(&self) -> Result<()> {
        let backend_session_id = self
            .backend_session_id
            .lock()
            .map_err(|_| dbgflow_core::DbgFlowError::Backend("test worker lock poisoned".into()))?
            .take();
        if let Some(backend_session_id) = backend_session_id {
            self.backend.close_session(&backend_session_id)
        } else {
            Ok(())
        }
    }

    fn kill(&self, _reason: &str) -> Result<()> {
        let backend_session_id = self
            .backend_session_id
            .lock()
            .map_err(|_| dbgflow_core::DbgFlowError::Backend("test worker lock poisoned".into()))?
            .clone();
        if let Some(backend_session_id) = backend_session_id {
            let _ = self.backend.cancel_session(&backend_session_id);
        }
        Ok(())
    }
}
