use dbgflow_core::backend::{
    BackendCapability, BackendInfo, BackendKind, BackendSession, CreateBackendSession,
    DebugBackend, DebugTarget, ExecuteBackendRequest, ExecuteBackendResult,
};
use dbgflow_core::logging::{LogEvent, LogSink};
use dbgflow_core::session::{CreateSession, ExecuteSession, SessionManager, SessionState};
use dbgflow_core::Result;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn mock_session_can_be_created_queried_and_closed() {
    let manager = SessionManager::with_mock_backend();

    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Mock,
            startup_timeout_ms: None,
        })
        .expect("create mock session");
    assert_eq!(session.state, SessionState::Starting);

    let session = wait_for_ready(&manager, session.id);

    let queried = manager.query_session(session.id).expect("query session");
    assert_eq!(queried.id, session.id);
    assert_eq!(queried.state, SessionState::Ready);

    let closed = manager.close_session(session.id).expect("close session");
    assert_eq!(closed.state, SessionState::Closing);

    let queried_after_close = wait_for_closed(&manager, session.id);
    assert_eq!(queried_after_close.state, SessionState::Closed);
}

#[test]
fn create_session_returns_existing_active_session_for_same_target() {
    let manager = SessionManager::with_mock_backend();

    let first = manager
        .create_session(CreateSession::default())
        .expect("create first session");
    let second = manager
        .create_session(CreateSession::default())
        .expect("return existing session");

    assert_eq!(second.id, first.id);

    let sessions = manager.list_sessions().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, first.id);
}

#[test]
fn create_session_does_not_reuse_closed_session() {
    let manager = SessionManager::with_mock_backend();

    let first = manager
        .create_session(CreateSession::default())
        .expect("create first session");
    manager
        .close_session(first.id)
        .expect("close first session");

    let second = manager
        .create_session(CreateSession::default())
        .expect("create replacement session");

    assert_ne!(second.id, first.id);
    let second = wait_for_ready(&manager, second.id);
    assert_eq!(second.state, SessionState::Ready);
}

#[test]
fn mock_session_execute_writes_output_artifact() {
    let root = test_artifact_root("mock-session-execute");
    let manager = SessionManager::with_artifact_root(
        vec![std::sync::Arc::new(
            dbgflow_core::backend::mock::MockBackend::new(),
        )],
        &root,
    );

    let session = manager
        .create_session(CreateSession::default())
        .expect("create session");
    let session = wait_for_ready(&manager, session.id);
    let result = manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: "k".to_string(),
            timeout_ms: None,
        })
        .expect("execute command");

    assert!(result.output.contains("mock executed: k"));
    assert!(result.artifact.path.is_file());
    let output = fs::read_to_string(result.artifact.path).expect("read output artifact");
    assert!(output.contains("mock executed: k"));
}

#[test]
fn mock_session_execute_rejects_denied_command() {
    let manager = SessionManager::with_mock_backend();
    let session = manager
        .create_session(CreateSession::default())
        .expect("create session");

    let error = manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: ".shell dir".to_string(),
            timeout_ms: None,
        })
        .expect_err("deny shell");

    assert!(error.to_string().contains("command denied"));
}

#[test]
fn attach_target_rejects_invalid_pid() {
    let manager = SessionManager::with_mock_backend();

    let zero = manager
        .create_session(CreateSession {
            target: DebugTarget::Attach { pid: 0 },
            startup_timeout_ms: None,
        })
        .expect_err("reject zero pid");
    assert!(zero.to_string().contains("attach pid"));

    let current = manager
        .create_session(CreateSession {
            target: DebugTarget::Attach {
                pid: std::process::id(),
            },
            startup_timeout_ms: None,
        })
        .expect_err("reject current process");
    assert!(current.to_string().contains("current dbgflow process"));
}

#[test]
fn launch_target_is_disabled_by_default() {
    let manager = SessionManager::with_mock_backend();
    let missing = std::env::temp_dir()
        .join(format!("dbgflow-missing-{}", std::process::id()))
        .join("missing.exe");

    let error = manager
        .create_session(CreateSession {
            target: DebugTarget::Launch {
                executable: missing,
                args: Vec::new(),
            },
            startup_timeout_ms: None,
        })
        .expect_err("reject disabled launch");

    assert!(error.to_string().contains("launch targets are disabled"));
}

#[test]
fn startup_timeout_logs_late_cleanup() {
    let logger = Arc::new(RecordingLogSink::default());
    let backend = Arc::new(SlowStartupBackend::default());
    let manager = SessionManager::with_artifact_root_and_logger(
        vec![backend.clone()],
        test_artifact_root("startup-timeout-logs"),
        logger.clone(),
    );

    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Mock,
            startup_timeout_ms: Some(1),
        })
        .expect("create session");

    let session = wait_for_error(&manager, session.id);
    assert_eq!(session.state, SessionState::Error);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while backend.close_count.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "late cleanup did not close backend session"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let events = logger.events();
    assert!(events
        .iter()
        .any(|event| event.event == "backend_startup_timed_out"));
    assert!(events
        .iter()
        .any(|event| event.event == "backend_startup_late_success_cleanup_finished"));
}

fn test_artifact_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create artifact root");
    root
}

fn wait_for_ready(
    manager: &SessionManager,
    session_id: dbgflow_core::session::SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Ready {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not become ready: {session:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_for_closed(
    manager: &SessionManager,
    session_id: dbgflow_core::session::SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Closed {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not close: {session:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_for_error(
    manager: &SessionManager,
    session_id: dbgflow_core::session::SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Error {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not become error: {session:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[derive(Default)]
struct RecordingLogSink {
    events: Mutex<Vec<LogEvent>>,
}

impl RecordingLogSink {
    fn events(&self) -> Vec<LogEvent> {
        self.events.lock().expect("events lock").clone()
    }
}

impl LogSink for RecordingLogSink {
    fn log(&self, event: LogEvent) {
        self.events.lock().expect("events lock").push(event);
    }
}

#[derive(Default)]
struct SlowStartupBackend {
    close_count: AtomicUsize,
}

impl DebugBackend for SlowStartupBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "mock".to_string(),
            kind: BackendKind::Mock,
            capabilities: vec![BackendCapability::SessionLifecycle],
        }
    }

    fn create_session(&self, _request: CreateBackendSession) -> Result<BackendSession> {
        std::thread::sleep(Duration::from_millis(50));
        Ok(BackendSession {
            id: "slow-backend-session".to_string(),
            warnings: Vec::new(),
        })
    }

    fn execute(&self, _request: ExecuteBackendRequest) -> Result<ExecuteBackendResult> {
        Ok(ExecuteBackendResult {
            output: String::new(),
            warnings: Vec::new(),
        })
    }

    fn close_session(&self, _backend_session_id: &str) -> Result<()> {
        self.close_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
