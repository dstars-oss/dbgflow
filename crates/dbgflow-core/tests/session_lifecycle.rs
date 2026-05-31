use dbgflow_core::backend::DebugTarget;
use dbgflow_core::session::{CreateSession, ExecuteSession, SessionManager, SessionState};
use std::fs;

#[test]
fn mock_session_can_be_created_queried_and_closed() {
    let manager = SessionManager::with_mock_backend();

    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Mock,
        })
        .expect("create mock session");
    assert_eq!(session.state, SessionState::Ready);

    let queried = manager.query_session(session.id).expect("query session");
    assert_eq!(queried.id, session.id);
    assert_eq!(queried.state, SessionState::Ready);

    let closed = manager.close_session(session.id).expect("close session");
    assert_eq!(closed.state, SessionState::Closed);

    let queried_after_close = manager
        .query_session(session.id)
        .expect("query closed session");
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
    let result = manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: "k".to_string(),
            timeout_ms: None,
        })
        .expect("execute command");

    assert!(result.output_preview.contains("mock executed: k"));
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
        })
        .expect_err("reject zero pid");
    assert!(zero.to_string().contains("attach pid"));

    let current = manager
        .create_session(CreateSession {
            target: DebugTarget::Attach {
                pid: std::process::id(),
            },
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
        })
        .expect_err("reject disabled launch");

    assert!(error.to_string().contains("launch targets are disabled"));
}

fn test_artifact_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create artifact root");
    root
}
