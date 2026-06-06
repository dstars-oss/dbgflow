use dbgflow_core::backend::{
    BackendCapability, BackendInfo, BackendKind, BackendSession, CreateBackendSession,
    DebugBackend, DebugTarget, ExecuteBackendRequest, ExecuteBackendResult,
};
use dbgflow_core::logging::{LogEvent, LogSink};
use dbgflow_core::session::{
    CreateSession, ExecuteSession, OperationStatus, SessionManager, SessionState,
};
use dbgflow_core::{DbgFlowError, Result};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
fn create_session_does_not_reuse_closing_session() {
    let backend = Arc::new(SlowCloseBackend::default());
    let manager = SessionManager::with_artifact_root(
        vec![backend],
        test_artifact_root("closing-session-retry"),
    );

    let first = manager
        .create_session(CreateSession::default())
        .expect("create first session");
    let first = wait_for_ready(&manager, first.id);

    let closing = manager
        .close_session(first.id)
        .expect("close first session");
    assert_eq!(closing.state, SessionState::Closing);

    let second = manager
        .create_session(CreateSession::default())
        .expect("create replacement session while first is closing");

    assert_ne!(second.id, first.id);
    let second = wait_for_ready(&manager, second.id);
    assert_eq!(second.state, SessionState::Ready);
}

#[test]
fn close_starting_session_closes_backend_after_startup_finishes() {
    let backend = Arc::new(SlowStartupBackend::default());
    let manager = SessionManager::with_artifact_root(
        vec![backend.clone()],
        test_artifact_root("close-starting-session"),
    );

    let session = manager
        .create_session(CreateSession::default())
        .expect("create session");
    let closing = manager.close_session(session.id).expect("close session");
    assert_eq!(closing.state, SessionState::Closing);

    let closed = wait_for_closed(&manager, session.id);
    assert_eq!(closed.state, SessionState::Closed);
    assert_eq!(backend.close_count.load(Ordering::SeqCst), 1);
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
    assert_eq!(
        result
            .session
            .last_operation
            .as_ref()
            .expect("last operation")
            .status,
        OperationStatus::Finished
    );
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
fn execute_sets_observable_operation_state() {
    let manager = SessionManager::with_artifact_root(
        vec![Arc::new(SlowExecuteBackend)],
        test_artifact_root("observable-execute-operation"),
    );
    let session = manager
        .create_session(CreateSession::default())
        .expect("create session");
    let session = wait_for_ready(&manager, session.id);
    let session_id = session.id;

    let execute_manager = manager.clone();
    let execute = std::thread::spawn(move || {
        execute_manager
            .execute(ExecuteSession {
                session_id,
                command: "!analyze -v".to_string(),
                timeout_ms: Some(1),
            })
            .expect("execute slow command")
    });

    let running = wait_for_current_operation(&manager, session_id);
    assert_eq!(running.current_operation.as_deref(), Some("!analyze -v"));
    let last = running.last_operation.as_ref().expect("last operation");
    assert_eq!(last.command, "!analyze -v");
    assert_eq!(last.status, OperationStatus::Running);

    let result = execute.join().expect("execute thread");
    assert_eq!(result.session.state, SessionState::Ready);
    let last = result
        .session
        .last_operation
        .as_ref()
        .expect("last operation");
    assert_eq!(last.status, OperationStatus::Finished);
    assert!(last.artifact.is_some());
    assert_eq!(last.output_bytes, Some(result.output.len()));
}

#[test]
fn close_session_cancels_running_operation_before_waiting_for_operation_lock() {
    let backend = Arc::new(CancellableExecuteBackend::default());
    let manager = SessionManager::with_artifact_root(
        vec![backend.clone()],
        test_artifact_root("close-cancels-running-operation"),
    );
    let session = manager
        .create_session(CreateSession::default())
        .expect("create session");
    let session = wait_for_ready(&manager, session.id);
    let session_id = session.id;

    let execute_manager = manager.clone();
    let execute = std::thread::spawn(move || {
        execute_manager.execute(ExecuteSession {
            session_id,
            command: "k".to_string(),
            timeout_ms: None,
        })
    });

    backend.wait_until_executing();
    let closing = manager.close_session(session_id).expect("close session");
    assert_eq!(closing.state, SessionState::Closing);
    let closed = wait_for_closed(&manager, session_id);
    assert_eq!(closed.state, SessionState::Closed);
    let last = closed.last_operation.as_ref().expect("last operation");
    assert_eq!(last.status, OperationStatus::Canceled);
    assert!(last.finished_at_unix_ms.is_some());
    assert!(last.duration_ms.is_some());
    assert!(last
        .error
        .as_deref()
        .is_some_and(|error| error.contains("close_session")));
    assert_eq!(backend.cancel_count.load(Ordering::SeqCst), 1);

    let execute_error = execute
        .join()
        .expect("execute thread")
        .expect_err("execute canceled");
    assert!(execute_error.to_string().contains("canceled"));
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
fn deprecated_timeout_fields_are_ignored_and_logged() {
    let logger = Arc::new(RecordingLogSink::default());
    let manager = SessionManager::with_artifact_root_and_logger(
        vec![Arc::new(SlowStartupBackend::default())],
        test_artifact_root("deprecated-timeouts"),
        logger.clone(),
    );

    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Mock,
            startup_timeout_ms: Some(1),
        })
        .expect("create session");

    let session = wait_for_ready(&manager, session.id);
    manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: "k".to_string(),
            timeout_ms: Some(1),
        })
        .expect("timeout field ignored");

    let events = logger.events();
    assert!(events
        .iter()
        .any(|event| event.event == "deprecated_startup_timeout_ignored"));
    assert!(events
        .iter()
        .any(|event| event.event == "deprecated_execute_timeout_ignored"));
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

fn wait_for_current_operation(
    manager: &SessionManager,
    session_id: dbgflow_core::session::SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.current_operation.is_some() {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not expose current operation: {session:?}"
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

#[derive(Default)]
struct SlowCloseBackend {
    next_id: AtomicUsize,
}

impl DebugBackend for SlowCloseBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "mock".to_string(),
            kind: BackendKind::Mock,
            capabilities: vec![BackendCapability::SessionLifecycle],
        }
    }

    fn create_session(&self, _request: CreateBackendSession) -> Result<BackendSession> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(BackendSession {
            id: format!("slow-close-backend-session-{id}"),
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
        std::thread::sleep(Duration::from_millis(250));
        Ok(())
    }
}

struct SlowExecuteBackend;

impl DebugBackend for SlowExecuteBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "mock".to_string(),
            kind: BackendKind::Mock,
            capabilities: vec![BackendCapability::SessionLifecycle],
        }
    }

    fn create_session(&self, _request: CreateBackendSession) -> Result<BackendSession> {
        Ok(BackendSession {
            id: "slow-execute-backend-session".to_string(),
            warnings: Vec::new(),
        })
    }

    fn execute(&self, request: ExecuteBackendRequest) -> Result<ExecuteBackendResult> {
        std::thread::sleep(Duration::from_millis(250));
        Ok(ExecuteBackendResult {
            output: format!("slow executed: {}", request.command),
            warnings: Vec::new(),
        })
    }

    fn close_session(&self, _backend_session_id: &str) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct CancellableExecuteBackend {
    state: Arc<(Mutex<CancellableState>, Condvar)>,
    cancel_count: AtomicUsize,
}

#[derive(Default)]
struct CancellableState {
    executing: bool,
    canceled: bool,
}

impl CancellableExecuteBackend {
    fn wait_until_executing(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().expect("cancellable state lock");
        while !state.executing {
            assert!(
                std::time::Instant::now() < deadline,
                "backend did not start executing"
            );
            let (next_state, _) = cvar
                .wait_timeout(state, Duration::from_millis(10))
                .expect("cancellable state wait");
            state = next_state;
        }
    }
}

impl DebugBackend for CancellableExecuteBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "mock".to_string(),
            kind: BackendKind::Mock,
            capabilities: vec![
                BackendCapability::SessionLifecycle,
                BackendCapability::Execute,
            ],
        }
    }

    fn create_session(&self, _request: CreateBackendSession) -> Result<BackendSession> {
        Ok(BackendSession {
            id: "cancellable-backend-session".to_string(),
            warnings: Vec::new(),
        })
    }

    fn execute(&self, _request: ExecuteBackendRequest) -> Result<ExecuteBackendResult> {
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().expect("cancellable state lock");
        state.executing = true;
        cvar.notify_all();
        while !state.canceled {
            state = cvar.wait(state).expect("cancellable state wait");
        }
        Err(DbgFlowError::Backend("canceled".to_string()))
    }

    fn cancel_session(&self, _backend_session_id: &str) -> Result<()> {
        self.cancel_count.fetch_add(1, Ordering::SeqCst);
        let (lock, cvar) = &*self.state;
        let mut state = lock.lock().expect("cancellable state lock");
        state.canceled = true;
        cvar.notify_all();
        Ok(())
    }

    fn close_session(&self, _backend_session_id: &str) -> Result<()> {
        Ok(())
    }
}
