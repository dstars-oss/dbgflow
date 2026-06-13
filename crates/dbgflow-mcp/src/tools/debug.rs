use super::registry::{AddSymbolsRequest, CreateSessionRequest, EvalRequest};
use dbgflow_common::process::ToolCallContext;
use dbgflow_common::{DbgFlowError, Result};
use dbgflow_debug::session::{
    CreateSession, EvalSession, EvalSessionResult, Session, SessionId, SessionManager,
};

pub(super) fn create_session(
    sessions: &SessionManager,
    request: CreateSessionRequest,
    context: ToolCallContext,
) -> Result<Session> {
    sessions.create_session_with_context(
        CreateSession {
            target: request.target,
            startup_timeout_ms: request.startup_timeout_ms,
        },
        context,
    )
}

pub(super) fn query_session(sessions: &SessionManager, session_id: SessionId) -> Result<Session> {
    sessions.query_session(session_id)
}

pub(super) fn list_sessions(sessions: &SessionManager) -> Result<Vec<Session>> {
    sessions.list_sessions()
}

pub(super) fn close_session(sessions: &SessionManager, session_id: SessionId) -> Result<Session> {
    sessions.close_session(session_id)
}

pub(super) fn eval(sessions: &SessionManager, request: EvalRequest) -> Result<EvalSessionResult> {
    sessions.eval(EvalSession {
        session_id: request.session_id,
        command: request.command,
        timeout_ms: request.timeout_ms,
    })
}

pub(super) fn add_symbols(
    sessions: &SessionManager,
    request: AddSymbolsRequest,
) -> Result<EvalSessionResult> {
    if request.paths.is_empty() {
        return Err(DbgFlowError::Backend(
            "at least one symbol path is required".to_string(),
        ));
    }

    let mut result = None;
    for path in &request.paths {
        let path = path.as_os_str().to_string_lossy();
        if path.trim().is_empty() {
            return Err(DbgFlowError::Backend(
                "symbol path must not be empty".to_string(),
            ));
        }
        if path
            .chars()
            .any(|ch| matches!(ch, '\r' | '\n' | '\u{2028}' | '\u{2029}') || ch.is_control())
        {
            return Err(DbgFlowError::Backend(
                "symbol path contains unsupported control characters".to_string(),
            ));
        }
        let command = format!(".sympath+ {path}");
        result = Some(eval(
            sessions,
            EvalRequest {
                session_id: request.session_id,
                command,
                timeout_ms: None,
            },
        )?);
    }

    result.ok_or_else(|| DbgFlowError::Backend("no symbol paths were applied".to_string()))
}
