#![cfg(windows)]

use dbgflow_core::backend::dbgeng::DbgEngBackend;
use dbgflow_core::backend::mock::MockBackend;
use dbgflow_core::backend::DebugTarget;
use dbgflow_core::session::{CreateSession, ExecuteSession, SessionManager, SessionState};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
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
        })
        .expect("attach process session");
    assert_eq!(session.state, SessionState::Break);

    let result = manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: "lm".to_string(),
            timeout_ms: Some(120_000),
        })
        .expect("query modules after attach");

    assert!(
        !result.output_preview.trim().is_empty(),
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
    let _launch_env = EnvVarGuard::set("DBGFLOW_ENABLE_LAUNCH", "1");
    let ping = ping_exe();
    let artifact_root = test_artifact_root("dbgeng-launch");
    let manager = dbgeng_manager(&artifact_root);
    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Launch {
                executable: ping,
                args: vec!["127.0.0.1".to_string(), "-n".to_string(), "3".to_string()],
            },
        })
        .expect("launch process session");
    assert_eq!(session.state, SessionState::Break);

    let result = manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: "g".to_string(),
            timeout_ms: Some(15_000),
        })
        .expect("continue launched process");

    assert_eq!(result.session.state, SessionState::Break);
    assert!(result.artifact.path.is_file());

    manager
        .close_session(session.id)
        .expect("close launch session");
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
    SessionManager::with_artifact_root(
        vec![
            std::sync::Arc::new(MockBackend::new()),
            std::sync::Arc::new(DbgEngBackend::new()),
        ],
        artifact_root,
    )
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

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}
