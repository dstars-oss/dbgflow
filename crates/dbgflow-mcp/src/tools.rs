use dbgflow_core::backend::DebugTarget;
use dbgflow_core::session::{
    CreateSession, ExecuteSession, ExecuteSessionResult, Session, SessionId, SessionManager,
};
use dbgflow_core::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::path::PathBuf;

pub const CREATE_SESSION: &str = "create_session";
pub const LIST_SESSIONS: &str = "list_sessions";
pub const CLOSE_SESSION: &str = "close_session";
pub const EXECUTE: &str = "execute";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
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
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "object",
                            "description": "Debug target. Omit for a mock session.",
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "mock" }
                                    },
                                    "required": ["kind"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "dump" },
                                        "path": {
                                            "type": "string",
                                            "description": "Path to a local Windows dump file."
                                        }
                                    },
                                    "required": ["kind", "path"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "attach" },
                                        "pid": {
                                            "type": "integer",
                                            "minimum": 1,
                                            "description": "Process id to attach."
                                        }
                                    },
                                    "required": ["kind", "pid"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "launch" },
                                        "executable": {
                                            "type": "string",
                                            "description": "Path to a local executable. Disabled unless DBGFLOW_ENABLE_LAUNCH=1."
                                        },
                                        "args": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "description": "Command-line arguments. Omit for no arguments."
                                        }
                                    },
                                    "required": ["kind", "executable"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    },
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: LIST_SESSIONS,
                description: "List debug sessions.",
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: CLOSE_SESSION,
                description: "Close a debug session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by create_session."
                        }
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: EXECUTE,
                description: "Execute an allowlisted debugger command in a session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by create_session."
                        },
                        "command": {
                            "type": "string",
                            "description": "Allowlisted debugger command."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Optional command timeout in milliseconds."
                        }
                    },
                    "required": ["session_id", "command"],
                    "additionalProperties": false
                }),
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

    pub fn execute(&self, request: ExecuteRequest) -> Result<ExecuteSessionResult> {
        self.sessions.execute(ExecuteSession {
            session_id: request.session_id,
            command: request.command,
            timeout_ms: request.timeout_ms,
        })
    }

    pub fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<Value, ToolCallError> {
        match name {
            CREATE_SESSION => self
                .create_session(decode_arguments(arguments)?)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            LIST_SESSIONS => self
                .list_sessions()
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            CLOSE_SESSION => {
                let request: CloseSessionRequest = decode_arguments(arguments)?;
                self.close_session(request.session_id)
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            EXECUTE => self
                .execute(decode_arguments(arguments)?)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            _ => Err(ToolCallError::invalid_request(format!(
                "unknown tool: {name}"
            ))),
        }
    }
}

#[derive(Debug)]
pub enum ToolCallError {
    InvalidRequest(String),
    Execution(String),
}

impl ToolCallError {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    fn execution(error: DbgFlowError) -> Self {
        Self::Execution(error.to_string())
    }
}

impl fmt::Display for ToolCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) | Self::Execution(message) => {
                formatter.write_str(message)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub session_id: SessionId,
    pub command: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct McpCreateSessionRequest {
    #[serde(default)]
    target: McpDebugTarget,
}

impl From<McpCreateSessionRequest> for CreateSessionRequest {
    fn from(value: McpCreateSessionRequest) -> Self {
        Self {
            target: value.target.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum McpDebugTarget {
    Mock,
    Dump {
        path: PathBuf,
    },
    Attach {
        pid: u32,
    },
    Launch {
        executable: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl Default for McpDebugTarget {
    fn default() -> Self {
        Self::Mock
    }
}

impl From<McpDebugTarget> for DebugTarget {
    fn from(value: McpDebugTarget) -> Self {
        match value {
            McpDebugTarget::Mock => DebugTarget::Mock,
            McpDebugTarget::Dump { path } => DebugTarget::Dump { path },
            McpDebugTarget::Attach { pid } => DebugTarget::Attach { pid },
            McpDebugTarget::Launch { executable, args } => DebugTarget::Launch { executable, args },
        }
    }
}

fn decode_arguments<T>(arguments: Value) -> std::result::Result<T, ToolCallError>
where
    T: for<'de> Deserialize<'de>,
{
    let arguments = match arguments {
        Value::Null => Value::Object(Default::default()),
        other => other,
    };
    serde_json::from_value(arguments)
        .map_err(|error| ToolCallError::invalid_request(format!("invalid tool arguments: {error}")))
}

fn to_value<T>(value: T) -> std::result::Result<Value, ToolCallError>
where
    T: Serialize,
{
    serde_json::to_value(value)
        .map_err(|error| ToolCallError::Execution(format!("serialize tool result: {error}")))
}

impl<'de> Deserialize<'de> for CreateSessionRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        McpCreateSessionRequest::deserialize(deserializer).map(Into::into)
    }
}
