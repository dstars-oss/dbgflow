use dbgflow_core::backend::DebugTarget;
use dbgflow_core::session::{
    CreateSession, EvalSession, EvalSessionResult, Session, SessionId, SessionManager,
};
use dbgflow_core::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;

pub const CREATE_SESSION: &str = "create_session";
pub const GET_SESSION: &str = "get_session";
pub const LIST_SESSIONS: &str = "list_sessions";
pub const CLOSE_SESSION: &str = "close_session";
pub const EVAL: &str = "eval";
pub const SET_SYMBOLS: &str = "set_symbols";

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
                    "description": "Example dump target: {\"target\":{\"kind\":\"dump\",\"path\":\"C:\\\\path\\\\file.dmp\"}}",
                    "properties": {
                        "target": {
                            "type": "object",
                            "description": "Debug target.",
                            "oneOf": [
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
                                            "description": "Path to a local executable."
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
                    "required": ["target"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: GET_SESSION,
                description: "Get the current state of a debug session.",
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
                name: EVAL,
                description: "Evaluate a native debugger command in a session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by create_session."
                        },
                        "command": {
                            "type": "string",
                            "description": "Native debugger command."
                        }
                    },
                    "required": ["session_id", "command"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: SET_SYMBOLS,
                description: "Set or append a native debugger symbol path.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by create_session."
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                            "description": "Debugger symbol path entries. Raw WinDbg symbol path strings are accepted."
                        },
                        "append": {
                            "type": "boolean",
                            "description": "Append to the current symbol path instead of replacing it."
                        }
                    },
                    "required": ["session_id", "paths"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    pub fn create_session(&self, request: CreateSessionRequest) -> Result<Session> {
        self.sessions.create_session(CreateSession {
            target: request.target,
            startup_timeout_ms: request.startup_timeout_ms,
        })
    }

    pub fn query_session(&self, session_id: SessionId) -> Result<Session> {
        self.sessions.query_session(session_id)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        self.sessions.list_sessions()
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<Session> {
        self.sessions.close_session(session_id)
    }

    pub fn eval(&self, request: EvalRequest) -> Result<EvalSessionResult> {
        self.sessions.eval(EvalSession {
            session_id: request.session_id,
            command: request.command,
            timeout_ms: request.timeout_ms,
        })
    }

    pub fn set_symbols(&self, request: SetSymbolsRequest) -> Result<EvalSessionResult> {
        if request.paths.is_empty() {
            return Err(DbgFlowError::Backend(
                "at least one symbol path is required".to_string(),
            ));
        }

        let append = request.append.unwrap_or(false);
        let mut result = None;
        for (index, path) in request.paths.iter().enumerate() {
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
            let command = if append || index > 0 {
                format!(".sympath+ {path}")
            } else {
                format!(".sympath {path}")
            };
            result = Some(self.eval(EvalRequest {
                session_id: request.session_id,
                command,
                timeout_ms: request.timeout_ms,
            })?);
        }

        result.ok_or_else(|| DbgFlowError::Backend("no symbol paths were applied".to_string()))
    }

    pub fn subscribe_session_updates(&self) -> mpsc::Receiver<SessionId> {
        self.sessions.subscribe_session_updates()
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
            GET_SESSION => {
                let request: GetSessionRequest = decode_arguments(arguments)?;
                self.query_session(request.session_id)
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
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
            EVAL => self
                .eval(decode_arguments(arguments)?)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            SET_SYMBOLS => self
                .set_symbols(decode_arguments(arguments)?)
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
    pub startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRequest {
    pub session_id: SessionId,
    pub command: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSymbolsRequest {
    pub session_id: SessionId,
    pub paths: Vec<PathBuf>,
    pub append: Option<bool>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct McpCreateSessionRequest {
    target: McpDebugTarget,
    startup_timeout_ms: Option<u64>,
}

impl From<McpCreateSessionRequest> for CreateSessionRequest {
    fn from(value: McpCreateSessionRequest) -> Self {
        Self {
            target: value.target.into(),
            startup_timeout_ms: value.startup_timeout_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum McpDebugTarget {
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

impl From<McpDebugTarget> for DebugTarget {
    fn from(value: McpDebugTarget) -> Self {
        match value {
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
