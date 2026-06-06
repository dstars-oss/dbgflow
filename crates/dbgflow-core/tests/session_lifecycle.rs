use dbgflow_core::backend::{CreateBackendSession, DebugTarget, ExecuteBackendResult};
use dbgflow_core::logging::{LogEvent, LogSink};
use dbgflow_core::session::worker::{SessionWorker, SessionWorkerLauncher, WorkerSession};
use dbgflow_core::session::{
    CreateSession, EvalSession, OperationStatus, SessionId, SessionManager, SessionState,
};
use dbgflow_core::{DbgFlowError, Result};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[test]
fn dump_session_can_be_created_queried_and_closed() {
    let manager = test_manager("create-query-close", WorkerBehavior::Normal);

    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("create-query-close"),
            startup_timeout_ms: None,
        })
        .expect("create dump session");
    assert_eq!(session.state, SessionState::Starting);

    let session = wait_for_break(&manager, session.id);

    let queried = manager.query_session(session.id).expect("query session");
    assert_eq!(queried.id, session.id);
    assert_eq!(queried.state, SessionState::Break);
    assert_eq!(queried.backend, "fake");
    assert!(queried.backend_session_id.is_some());

    let closed = manager.close_session(session.id).expect("close session");
    assert_eq!(closed.state, SessionState::Closing);

    let queried_after_close = wait_for_closed(&manager, session.id);
    assert_eq!(queried_after_close.state, SessionState::Closed);
}

#[test]
fn create_session_returns_existing_active_session_for_same_target() {
    let manager = test_manager("reuse-active", WorkerBehavior::Normal);
    let target = test_dump_target("reuse-active-target");

    let first = manager
        .create_session(CreateSession {
            target: target.clone(),
            startup_timeout_ms: None,
        })
        .expect("create first session");
    let second = manager
        .create_session(CreateSession {
            target,
            startup_timeout_ms: None,
        })
        .expect("return existing session");

    assert_eq!(second.id, first.id);

    let sessions = manager.list_sessions().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, first.id);
}

#[test]
fn create_session_does_not_reuse_closed_session() {
    let manager = test_manager("closed-retry", WorkerBehavior::Normal);
    let target = test_dump_target("closed-retry-target");

    let first = manager
        .create_session(CreateSession {
            target: target.clone(),
            startup_timeout_ms: None,
        })
        .expect("create first session");
    let first = wait_for_break(&manager, first.id);
    manager
        .close_session(first.id)
        .expect("close first session");
    wait_for_closed(&manager, first.id);

    let second = manager
        .create_session(CreateSession {
            target,
            startup_timeout_ms: None,
        })
        .expect("create replacement session");

    assert_ne!(second.id, first.id);
    let second = wait_for_break(&manager, second.id);
    assert_eq!(second.state, SessionState::Break);
}

#[test]
fn create_session_does_not_reuse_closing_session() {
    let manager = test_manager(
        "closing-retry",
        WorkerBehavior::SlowClose(Duration::from_millis(250)),
    );
    let target = test_dump_target("closing-retry-target");

    let first = manager
        .create_session(CreateSession {
            target: target.clone(),
            startup_timeout_ms: None,
        })
        .expect("create first session");
    let first = wait_for_break(&manager, first.id);

    let closing = manager
        .close_session(first.id)
        .expect("close first session");
    assert_eq!(closing.state, SessionState::Closing);

    let second = manager
        .create_session(CreateSession {
            target,
            startup_timeout_ms: None,
        })
        .expect("create replacement session while first is closing");

    assert_ne!(second.id, first.id);
    let second = wait_for_break(&manager, second.id);
    assert_eq!(second.state, SessionState::Break);
}

#[test]
fn close_starting_session_kills_worker_after_startup_finishes() {
    let close_count = Arc::new(AtomicUsize::new(0));
    let kill_count = Arc::new(AtomicUsize::new(0));
    let manager = test_manager(
        "close-starting",
        WorkerBehavior::SlowCreate {
            delay: Duration::from_millis(50),
            close_count: close_count.clone(),
            kill_count: kill_count.clone(),
        },
    );

    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("close-starting-target"),
            startup_timeout_ms: None,
        })
        .expect("create session");
    let closing = manager.close_session(session.id).expect("close session");
    assert_eq!(closing.state, SessionState::Closing);

    let closed = wait_for_closed(&manager, session.id);
    assert_eq!(closed.state, SessionState::Closed);
    assert_eq!(close_count.load(Ordering::SeqCst), 0);
    assert_eq!(kill_count.load(Ordering::SeqCst), 1);
}

#[test]
fn session_eval_writes_output_artifact() {
    let root = test_artifact_root("execute-artifact");
    let manager = SessionManager::with_worker_launcher(
        Arc::new(TestWorkerLauncher::new(WorkerBehavior::Normal)),
        &root,
    );

    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("execute-artifact-target"),
            startup_timeout_ms: None,
        })
        .expect("create session");
    let session = wait_for_break(&manager, session.id);
    let result = manager
        .eval(EvalSession {
            session_id: session.id,
            command: "k".to_string(),
            timeout_ms: None,
        })
        .expect("execute command");

    assert!(result.output.contains("fake worker executed: k"));
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
    assert!(output.contains("fake worker executed: k"));
}

#[test]
fn eval_rejects_denied_command() {
    let manager = test_manager("deny-command", WorkerBehavior::Normal);
    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("deny-command-target"),
            startup_timeout_ms: None,
        })
        .expect("create session");

    let error = manager
        .eval(EvalSession {
            session_id: session.id,
            command: ".shell dir".to_string(),
            timeout_ms: None,
        })
        .expect_err("deny shell");

    assert!(error.to_string().contains("command denied"));
}

#[test]
fn eval_sets_observable_operation_state() {
    let manager = test_manager(
        "observable-execute",
        WorkerBehavior::SlowExecute(Duration::from_millis(250)),
    );
    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("observable-execute-target"),
            startup_timeout_ms: None,
        })
        .expect("create session");
    let session = wait_for_break(&manager, session.id);
    let session_id = session.id;

    let eval_manager = manager.clone();
    let eval = std::thread::spawn(move || {
        eval_manager
            .eval(EvalSession {
                session_id,
                command: "!analyze -v".to_string(),
                timeout_ms: Some(1),
            })
            .expect("eval slow command")
    });

    let running = wait_for_current_operation(&manager, session_id);
    assert_eq!(running.current_operation.as_deref(), Some("!analyze -v"));
    let last = running.last_operation.as_ref().expect("last operation");
    assert_eq!(last.command, "!analyze -v");
    assert_eq!(last.status, OperationStatus::Running);

    let result = eval.join().expect("eval thread");
    assert_eq!(result.session.state, SessionState::Break);
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
fn close_session_kills_running_worker_before_waiting_for_operation_lock() {
    let state = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
    let kill_count = Arc::new(AtomicUsize::new(0));
    let manager = test_manager(
        "close-running",
        WorkerBehavior::BlockingExecute {
            state: state.clone(),
            kill_count: kill_count.clone(),
        },
    );
    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("close-running-target"),
            startup_timeout_ms: None,
        })
        .expect("create session");
    let session = wait_for_break(&manager, session.id);
    let session_id = session.id;

    let eval_manager = manager.clone();
    let eval = std::thread::spawn(move || {
        eval_manager.eval(EvalSession {
            session_id,
            command: "k".to_string(),
            timeout_ms: None,
        })
    });

    wait_until_executing(&state);
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
    assert_eq!(kill_count.load(Ordering::SeqCst), 1);

    let eval_error = eval
        .join()
        .expect("eval thread")
        .expect_err("eval canceled");
    assert!(eval_error.to_string().contains("canceled"));
}

#[test]
fn idle_worker_exit_marks_session_error() {
    let exited = Arc::new(AtomicBool::new(false));
    let manager = test_manager(
        "idle-worker-exit",
        WorkerBehavior::ExitAfterCreate {
            exited: exited.clone(),
        },
    );
    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("idle-worker-exit-target"),
            startup_timeout_ms: None,
        })
        .expect("create session");
    let session = wait_for_break(&manager, session.id);

    exited.store(true, Ordering::SeqCst);
    let error = wait_for_error(&manager, session.id);
    assert_eq!(error.state, SessionState::Error);
    assert!(error
        .error
        .as_deref()
        .is_some_and(|error| error.contains("worker exited")));
}

#[test]
fn dropping_manager_kills_active_workers() {
    let kill_count = Arc::new(AtomicUsize::new(0));
    {
        let manager = test_manager(
            "drop-manager-kills-workers",
            WorkerBehavior::TrackKill {
                kill_count: kill_count.clone(),
            },
        );
        let session = manager
            .create_session(CreateSession {
                target: test_dump_target("drop-manager-kills-workers-target"),
                startup_timeout_ms: None,
            })
            .expect("create session");
        let _ = wait_for_break(&manager, session.id);
    }

    assert_eq!(kill_count.load(Ordering::SeqCst), 1);
}

#[test]
fn attach_target_rejects_invalid_pid() {
    let manager = test_manager("invalid-attach", WorkerBehavior::Normal);

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
    let manager = test_manager("launch-disabled", WorkerBehavior::Normal);
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
    let manager = SessionManager::with_worker_launcher_and_logger(
        Arc::new(TestWorkerLauncher::new(WorkerBehavior::Normal)),
        test_artifact_root("deprecated-timeouts"),
        logger.clone(),
    );

    let session = manager
        .create_session(CreateSession {
            target: test_dump_target("deprecated-timeouts-target"),
            startup_timeout_ms: Some(1),
        })
        .expect("create session");

    let session = wait_for_break(&manager, session.id);
    manager
        .eval(EvalSession {
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
        .any(|event| event.event == "deprecated_eval_timeout_ignored"));
}

fn test_manager(name: &str, behavior: WorkerBehavior) -> SessionManager {
    SessionManager::with_worker_launcher(
        Arc::new(TestWorkerLauncher::new(behavior)),
        test_artifact_root(name),
    )
}

fn test_artifact_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create artifact root");
    root
}

fn test_dump_target(name: &str) -> DebugTarget {
    let root = test_artifact_root(name);
    let path = root.join("input.dmp");
    fs::write(&path, b"not a real dump").expect("write fake dump");
    DebugTarget::Dump { path }
}

fn wait_for_break(
    manager: &SessionManager,
    session_id: SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Break {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not break: {session:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_closed(
    manager: &SessionManager,
    session_id: SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Closed {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not close: {session:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_error(
    manager: &SessionManager,
    session_id: SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Error {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not enter error: {session:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_current_operation(
    manager: &SessionManager,
    session_id: SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.current_operation.is_some() {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not expose current operation: {session:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until_executing(state: &Arc<(Mutex<BlockingState>, Condvar)>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let (lock, cvar) = &**state;
    let mut state = lock.lock().expect("blocking state lock");
    while !state.executing {
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not start executing"
        );
        let (next_state, _) = cvar
            .wait_timeout(state, Duration::from_millis(10))
            .expect("blocking state wait");
        state = next_state;
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

#[derive(Clone)]
enum WorkerBehavior {
    Normal,
    SlowCreate {
        delay: Duration,
        close_count: Arc<AtomicUsize>,
        kill_count: Arc<AtomicUsize>,
    },
    SlowClose(Duration),
    SlowExecute(Duration),
    BlockingExecute {
        state: Arc<(Mutex<BlockingState>, Condvar)>,
        kill_count: Arc<AtomicUsize>,
    },
    ExitAfterCreate {
        exited: Arc<AtomicBool>,
    },
    TrackKill {
        kill_count: Arc<AtomicUsize>,
    },
}

struct TestWorkerLauncher {
    next_id: AtomicUsize,
    behavior: WorkerBehavior,
}

impl TestWorkerLauncher {
    fn new(behavior: WorkerBehavior) -> Self {
        Self {
            next_id: AtomicUsize::new(0),
            behavior,
        }
    }
}

impl SessionWorkerLauncher for TestWorkerLauncher {
    fn spawn(
        &self,
        _session_id: SessionId,
        _logger: Arc<dyn LogSink>,
    ) -> Result<Arc<dyn SessionWorker>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(TestWorker {
            backend_session_id: format!("fake-session-{id}"),
            behavior: self.behavior.clone(),
            closed: AtomicBool::new(false),
        }))
    }
}

struct TestWorker {
    backend_session_id: String,
    behavior: WorkerBehavior,
    closed: AtomicBool,
}

impl SessionWorker for TestWorker {
    fn create_session(&self, _request: CreateBackendSession) -> Result<WorkerSession> {
        if let WorkerBehavior::SlowCreate { delay, .. } = &self.behavior {
            std::thread::sleep(*delay);
        }
        Ok(WorkerSession {
            backend: "fake".to_string(),
            backend_session_id: self.backend_session_id.clone(),
            warnings: Vec::new(),
        })
    }

    fn execute(&self, command: String) -> Result<ExecuteBackendResult> {
        match &self.behavior {
            WorkerBehavior::SlowExecute(delay) => std::thread::sleep(*delay),
            WorkerBehavior::BlockingExecute { state, .. } => {
                let (lock, cvar) = &**state;
                let mut state = lock.lock().expect("blocking state lock");
                state.executing = true;
                cvar.notify_all();
                while !state.canceled {
                    state = cvar.wait(state).expect("blocking state wait");
                }
                return Err(DbgFlowError::Backend("canceled".to_string()));
            }
            _ => {}
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbgFlowError::Backend("worker closed".to_string()));
        }
        Ok(ExecuteBackendResult {
            output: format!("fake worker executed: {command}"),
            warnings: Vec::new(),
        })
    }

    fn close(&self) -> Result<()> {
        match &self.behavior {
            WorkerBehavior::SlowCreate { close_count, .. } => {
                close_count.fetch_add(1, Ordering::SeqCst);
            }
            WorkerBehavior::SlowClose(delay) => {
                std::thread::sleep(*delay);
            }
            _ => {}
        }
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn has_exited(&self) -> Result<bool> {
        match &self.behavior {
            WorkerBehavior::ExitAfterCreate { exited } => Ok(exited.load(Ordering::SeqCst)),
            _ => Ok(self.closed.load(Ordering::SeqCst)),
        }
    }

    fn kill(&self, _reason: &str) -> Result<()> {
        match &self.behavior {
            WorkerBehavior::SlowCreate { kill_count, .. } => {
                kill_count.fetch_add(1, Ordering::SeqCst);
            }
            WorkerBehavior::BlockingExecute { state, kill_count } => {
                kill_count.fetch_add(1, Ordering::SeqCst);
                let (lock, cvar) = &**state;
                let mut state = lock.lock().expect("blocking state lock");
                state.canceled = true;
                cvar.notify_all();
            }
            WorkerBehavior::TrackKill { kill_count } => {
                kill_count.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct BlockingState {
    executing: bool,
    canceled: bool,
}
