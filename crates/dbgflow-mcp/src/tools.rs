use dbgflow_core::backend::DebugTarget;
use dbgflow_core::session::{CreateSession, Session, SessionId, SessionManager};
use dbgflow_core::Result;
use serde::{Deserialize, Serialize};

pub const CREATE_SESSION: &str = "create_session";
pub const LIST_SESSIONS: &str = "list_sessions";
pub const CLOSE_SESSION: &str = "close_session";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
}

#[derive(Clone)]
pub struct ToolService {
    sessions: SessionManager,
}

impl ToolService {
    pub fn new(sessions: SessionManager) -> Self {
        Self { sessions }
    }

    pub fn tool_descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor {
                name: CREATE_SESSION,
                description:
                    "Create a debug session or return an existing session for the same target.",
            },
            ToolDescriptor {
                name: LIST_SESSIONS,
                description: "List debug sessions.",
            },
            ToolDescriptor {
                name: CLOSE_SESSION,
                description: "Close a debug session.",
            },
        ]
    }

    pub fn create_session(&self, request: CreateSessionRequest) -> Result<Session> {
        self.sessions.create_session(CreateSession {
            target: request.target,
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        self.sessions.list_sessions()
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<Session> {
        self.sessions.close_session(session_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub target: DebugTarget,
}

impl Default for CreateSessionRequest {
    fn default() -> Self {
        Self {
            target: DebugTarget::Mock,
        }
    }
}
