use crate::session::SessionId;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, DbgFlowError>;

#[derive(Debug, Error)]
pub enum DbgFlowError {
    #[error("backend not found: {0}")]
    BackendNotFound(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    #[error("session is closed: {0}")]
    SessionClosed(SessionId),

    #[error("command denied by policy: {command}; reason: {reason}")]
    CommandDenied { command: String, reason: String },

    #[error("artifact error: {0}")]
    Artifact(String),
}
