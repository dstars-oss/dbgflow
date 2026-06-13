use dbgflow_core::backend::DebugTarget;
use dbgflow_core::profile::{ProfileCollectorConfig, ProfileManager, ProfileResult, RunProfile};
use dbgflow_core::session::{
    CreateSession, EvalSession, EvalSessionResult, Session, SessionId, SessionManager,
};
use dbgflow_core::ttd::{RecordTtd, TtdRecordingManager, TtdRecordingResult};
use dbgflow_core::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;

pub const CREATE_SESSION: &str = "dbg.create_session";
pub const GET_SESSION: &str = "dbg.get_session";
pub const LIST_SESSIONS: &str = "dbg.list_sessions";
pub const CLOSE_SESSION: &str = "dbg.close_session";
pub const EVAL: &str = "dbg.eval";
pub const ADD_SYMBOLS: &str = "dbg.add_symbols";
pub const RECORD_PROFILE: &str = "trace.record_profile";
pub const RECORD_TTD: &str = "trace.record_ttd";

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
    profiles: ProfileManager,
    ttd_recordings: TtdRecordingManager,
}

impl ToolService {
    pub fn new(sessions: SessionManager) -> Self {
        Self {
            sessions,
            profiles: ProfileManager::new("artifacts"),
            ttd_recordings: TtdRecordingManager::new("artifacts"),
        }
    }

    pub fn with_profiles(sessions: SessionManager, profiles: ProfileManager) -> Self {
        Self {
            sessions,
            profiles,
            ttd_recordings: TtdRecordingManager::new("artifacts"),
        }
    }

    pub fn with_profiles_and_ttd(
        sessions: SessionManager,
        profiles: ProfileManager,
        ttd_recordings: TtdRecordingManager,
    ) -> Self {
        Self {
            sessions,
            profiles,
            ttd_recordings,
        }
    }

    #[cfg(test)]
    fn new_for_tests() -> Self {
        let root = std::env::temp_dir().join(format!("dbgflow-mcp-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        Self {
            sessions: SessionManager::with_artifact_root(&root),
            profiles: ProfileManager::new(&root),
            ttd_recordings: TtdRecordingManager::new(&root),
        }
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
                            "description": "Session id returned by dbg.create_session."
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
                            "description": "Session id returned by dbg.create_session."
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
                            "description": "Session id returned by dbg.create_session."
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
                name: ADD_SYMBOLS,
                description: "Append native debugger symbol path entries to a session.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session id returned by dbg.create_session."
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                            "description": "Debugger symbol path entries. Raw WinDbg symbol path strings are accepted."
                        }
                    },
                    "required": ["session_id", "paths"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: RECORD_PROFILE,
                description:
                    "Launch a process and record a native ETW profile trace as a standard ETL artifact. Procmon collectors use the server runtime's configured Sysinternals directory.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
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
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Stop collection when the target exits or this timeout expires."
                        },
                        "collectors": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "type": "string", "const": "native_etw" },
                                            "scope": {
                                                "type": "object",
                                                "properties": {
                                                    "kind": { "type": "string", "const": "target_process" }
                                                },
                                                "required": ["kind"],
                                                "additionalProperties": false
                                            },
                                            "event_sets": {
                                                "type": "array",
                                                "items": { "type": "string", "enum": ["process", "file_io"] },
                                                "minItems": 1
                                            },
                                            "stacks": {
                                                "type": "object",
                                                "properties": {
                                                    "enabled": { "type": "boolean" }
                                                },
                                                "additionalProperties": false
                                            }
                                        },
                                        "required": ["kind", "scope", "event_sets"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "type": "string", "const": "procmon" },
                                            "capture_stacks": { "type": "boolean" },
                                            "filters": {
                                                "type": "object",
                                                "properties": {
                                                    "operations": {
                                                        "type": "array",
                                                        "items": { "type": "string" }
                                                    },
                                                    "paths": {
                                                        "type": "array",
                                                        "items": { "type": "string" }
                                                    }
                                                },
                                                "additionalProperties": false
                                            }
                                        },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    }
                                ]
                            },
                            "description": "Collectors to run around the same launched target. Omit to use native_etw target_process process and file_io with stacks enabled."
                        }
                    },
                    "required": ["target", "timeout_ms"],
                    "additionalProperties": false
                }),
            },
            ToolDescriptor {
                name: RECORD_TTD,
                description:
                    "Record a Time Travel Debugging trace with TTD.exe. Supports launch, attach, and bounded monitor recording into controlled artifacts.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "object",
                            "oneOf": [
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
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "attach" },
                                        "pid": {
                                            "type": "integer",
                                            "minimum": 1,
                                            "description": "Process id to attach and record."
                                        }
                                    },
                                    "required": ["kind", "pid"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": { "type": "string", "const": "monitor" },
                                        "program": {
                                            "type": "string",
                                            "description": "Executable file name or absolute executable path to monitor."
                                        },
                                        "cmd_line_filter": {
                                            "type": "string",
                                            "description": "Optional command-line substring filter for monitor mode."
                                        }
                                    },
                                    "required": ["kind", "program"],
                                    "additionalProperties": false
                                }
                            ]
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Stop recording when the recorder exits or this timeout expires."
                        },
                        "options": {
                            "type": "object",
                            "properties": {
                                "children": { "type": "boolean" },
                                "no_ui": { "type": "boolean" },
                                "accept_eula": { "type": "boolean" },
                                "ring": { "type": "boolean" },
                                "max_file_mb": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Maximum TTD trace size in MiB."
                                },
                                "modules": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                },
                                "record_mode": {
                                    "type": "string",
                                    "enum": ["automatic", "manual"]
                                },
                                "replay_cpu_support": {
                                    "type": "string",
                                    "enum": [
                                        "default",
                                        "most_conservative",
                                        "most_aggressive",
                                        "intel_avx_required",
                                        "intel_avx2_required"
                                    ]
                                }
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["target", "timeout_ms"],
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

    pub fn add_symbols(&self, request: AddSymbolsRequest) -> Result<EvalSessionResult> {
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
            result = Some(self.eval(EvalRequest {
                session_id: request.session_id,
                command,
                timeout_ms: None,
            })?);
        }

        result.ok_or_else(|| DbgFlowError::Backend("no symbol paths were applied".to_string()))
    }

    pub fn run_profile(&self, request: RunProfileRequest) -> Result<ProfileResult> {
        self.profiles.run_profile(request.into())
    }

    pub fn record_ttd(&self, request: RecordTtd) -> Result<TtdRecordingResult> {
        self.ttd_recordings.record_ttd(request)
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
            ADD_SYMBOLS => self
                .add_symbols(decode_arguments(arguments)?)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            RECORD_PROFILE => self
                .run_profile(decode_arguments(arguments)?)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            RECORD_TTD => self
                .record_ttd(decode_arguments(arguments)?)
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
#[serde(deny_unknown_fields)]
pub struct AddSymbolsRequest {
    pub session_id: SessionId,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProfileRequest {
    pub target: dbgflow_core::profile::ProfileTarget,
    pub timeout_ms: u64,
    pub collectors: Vec<ProfileCollectorConfig>,
}

impl From<RunProfileRequest> for RunProfile {
    fn from(value: RunProfileRequest) -> Self {
        Self {
            target: value.target,
            timeout_ms: value.timeout_ms,
            collectors: value.collectors,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRunProfileRequest {
    target: dbgflow_core::profile::ProfileTarget,
    timeout_ms: u64,
    #[serde(default)]
    collectors: Option<Vec<ProfileCollectorConfig>>,
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

impl<'de> Deserialize<'de> for RunProfileRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawRunProfileRequest::deserialize(deserializer)?;
        let collectors = match raw.collectors {
            None => vec![ProfileCollectorConfig::default()],
            Some(collectors) => {
                if collectors.is_empty() {
                    return Err(serde::de::Error::custom(
                        "collectors must contain at least one collector",
                    ));
                }
                collectors
            }
        };
        Ok(Self {
            target: raw.target,
            timeout_ms: raw.timeout_ms,
            collectors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbgflow_core::profile::{
        EtwEventSet, EtwProfileScope, EtwStackConfig, ProfileCollectorKind,
    };
    use dbgflow_core::ttd::{
        RecordTtd, TtdRecordMode, TtdRecordingOptions, TtdReplayCpuSupport, TtdTarget,
    };

    #[test]
    fn tool_descriptors_include_record_profile() {
        let service = ToolService::new_for_tests();

        let descriptors = service.tool_descriptors();
        let record_profile = descriptors
            .iter()
            .find(|descriptor| descriptor.name == RECORD_PROFILE)
            .expect("trace.record_profile descriptor");

        assert!(record_profile.description.contains("profile"));
        assert_eq!(record_profile.input_schema["type"], "object");
        let event_set_enum = record_profile.input_schema["properties"]["collectors"]["items"]
            ["oneOf"][0]["properties"]["event_sets"]["items"]["enum"]
            .as_array()
            .expect("event set enum");
        assert!(event_set_enum.contains(&Value::String("process".to_string())));
        assert!(event_set_enum.contains(&Value::String("file_io".to_string())));
        assert!(!event_set_enum.contains(&Value::String("process_lifecycle".to_string())));
    }

    #[test]
    fn tool_descriptors_include_record_ttd() {
        let service = ToolService::new_for_tests();

        let descriptors = service.tool_descriptors();
        let record_ttd = descriptors
            .iter()
            .find(|descriptor| descriptor.name == RECORD_TTD)
            .expect("trace.record_ttd descriptor");

        assert!(record_ttd.description.contains("TTD"));
        assert_eq!(record_ttd.input_schema["type"], "object");
        assert!(record_ttd.input_schema["properties"]["target"]["oneOf"]
            .as_array()
            .expect("target variants")
            .iter()
            .any(|target| target["properties"]["kind"]["const"] == "monitor"));
    }

    #[test]
    fn run_profile_arguments_decode_to_launch_target_and_native_etw() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe",
                "args": ["/C", "echo dbgflow"]
            },
            "timeout_ms": 1000,
            "collectors": [
                {
                    "kind": "native_etw",
                    "scope": { "kind": "target_process" },
                    "event_sets": ["process"],
                    "stacks": { "enabled": true }
                }
            ]
        });

        let request: RunProfileRequest = decode_arguments(value).expect("decode request");
        assert_eq!(request.timeout_ms, 1000);
        assert_eq!(request.collectors.len(), 1);
        assert_eq!(
            request.collectors[0].kind(),
            ProfileCollectorKind::NativeEtw
        );
        assert!(matches!(
            request.collectors[0],
            ProfileCollectorConfig::NativeEtw {
                scope: EtwProfileScope::TargetProcess,
                ref event_sets,
                stacks: EtwStackConfig { enabled: true }
            } if event_sets == &vec![EtwEventSet::Process]
        ));
    }

    #[test]
    fn run_profile_arguments_decode_default_native_etw() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000
        });

        let request: RunProfileRequest = decode_arguments(value).expect("decode request");
        assert_eq!(request.collectors.len(), 1);
        assert!(matches!(
            request.collectors[0],
            ProfileCollectorConfig::NativeEtw {
                scope: EtwProfileScope::TargetProcess,
                ref event_sets,
                stacks: EtwStackConfig { enabled: true }
            } if event_sets == &vec![EtwEventSet::Process, EtwEventSet::FileIo]
        ));
    }

    #[test]
    fn run_profile_arguments_decode_native_etw_file_io_event_set() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": [
                {
                    "kind": "native_etw",
                    "scope": { "kind": "target_process" },
                    "event_sets": ["process", "file_io"]
                }
            ]
        });

        let request: RunProfileRequest = decode_arguments(value).expect("decode request");

        assert!(matches!(
            request.collectors[0],
            ProfileCollectorConfig::NativeEtw {
                scope: EtwProfileScope::TargetProcess,
                ref event_sets,
                stacks: EtwStackConfig { enabled: true }
            } if event_sets == &vec![EtwEventSet::Process, EtwEventSet::FileIo]
        ));
    }

    #[test]
    fn run_profile_arguments_reject_legacy_process_lifecycle_event_set() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": [
                {
                    "kind": "native_etw",
                    "scope": { "kind": "target_process" },
                    "event_sets": ["process_lifecycle"]
                }
            ]
        });

        let error =
            decode_arguments::<RunProfileRequest>(value).expect_err("reject old event set name");

        assert!(error.to_string().contains("process_lifecycle"));
    }

    #[test]
    fn run_profile_arguments_reject_legacy_collector() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collector": {
                "kind": "native_etw",
                "preset": "system_overview"
            }
        });

        let error =
            decode_arguments::<RunProfileRequest>(value).expect_err("reject legacy collector");
        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("collector"));
    }

    #[test]
    fn run_profile_arguments_reject_legacy_native_etw_preset() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": [
                {
                    "kind": "native_etw",
                    "preset": "system_overview"
                }
            ]
        });

        let error = decode_arguments::<RunProfileRequest>(value).expect_err("reject legacy preset");
        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("preset"));
    }

    #[test]
    fn run_profile_arguments_decode_procmon_collectors_array() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": [
                {
                    "kind": "native_etw",
                    "scope": { "kind": "target_process" },
                    "event_sets": ["process"]
                },
                {
                    "kind": "procmon",
                    "capture_stacks": true,
                    "filters": {
                        "operations": ["CreateFile", "ReadFile"],
                        "paths": ["C:\\data\\large_input.bin"]
                    }
                }
            ]
        });

        let request: RunProfileRequest = decode_arguments(value).expect("decode request");
        assert_eq!(request.collectors.len(), 2);
        assert!(matches!(
            request.collectors[1],
            ProfileCollectorConfig::Procmon { .. }
        ));
    }

    #[test]
    fn run_profile_arguments_reject_empty_collectors_array() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": []
        });

        let error = decode_arguments::<RunProfileRequest>(value).expect_err("reject empty array");
        assert!(error
            .to_string()
            .contains("collectors must contain at least one collector"));
    }

    #[test]
    fn run_profile_arguments_reject_top_level_sysinternals_dir() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "sysinternals_dir": "C:\\Sysinternals"
        });

        let error = decode_arguments::<RunProfileRequest>(value)
            .expect_err("reject top-level sysinternals_dir");

        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("sysinternals_dir"));
    }

    #[test]
    fn run_profile_arguments_reject_procmon_sysinternals_dir() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": [
                {
                    "kind": "procmon",
                    "sysinternals_dir": "C:\\Sysinternals"
                }
            ]
        });

        let error = decode_arguments::<RunProfileRequest>(value)
            .expect_err("reject procmon sysinternals_dir");

        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("sysinternals_dir"));
    }

    #[test]
    fn record_ttd_arguments_decode_launch_with_options() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe",
                "args": ["/C", "echo dbgflow"]
            },
            "timeout_ms": 1000,
            "options": {
                "accept_eula": true,
                "ring": true,
                "max_file_mb": 256,
                "modules": ["cmd.exe"],
                "record_mode": "manual",
                "replay_cpu_support": "intel_avx2_required"
            }
        });

        let request: RecordTtd = decode_arguments(value).expect("decode record_ttd");

        assert_eq!(request.timeout_ms, 1000);
        assert!(matches!(request.target, TtdTarget::Launch { .. }));
        assert_eq!(
            request.options,
            TtdRecordingOptions {
                accept_eula: true,
                ring: true,
                max_file_mb: 256,
                modules: vec!["cmd.exe".to_string()],
                record_mode: TtdRecordMode::Manual,
                replay_cpu_support: TtdReplayCpuSupport::IntelAvx2Required,
                ..Default::default()
            }
        );
    }

    #[test]
    fn record_ttd_arguments_decode_attach_with_default_options() {
        let value = json!({
            "target": {
                "kind": "attach",
                "pid": 1234
            },
            "timeout_ms": 1000
        });

        let request: RecordTtd = decode_arguments(value).expect("decode record_ttd");

        assert!(matches!(request.target, TtdTarget::Attach { pid: 1234 }));
        assert_eq!(request.options, TtdRecordingOptions::default());
    }

    #[test]
    fn record_ttd_arguments_decode_monitor() {
        let value = json!({
            "target": {
                "kind": "monitor",
                "program": "notepad.exe",
                "cmd_line_filter": "specialfile.txt"
            },
            "timeout_ms": 1000
        });

        let request: RecordTtd = decode_arguments(value).expect("decode record_ttd");

        assert!(matches!(
            request.target,
            TtdTarget::Monitor {
                ref program,
                ref cmd_line_filter
            } if program == std::path::Path::new("notepad.exe")
                && cmd_line_filter.as_deref() == Some("specialfile.txt")
        ));
    }

    #[test]
    fn record_ttd_arguments_reject_unknown_fields() {
        let value = json!({
            "target": {
                "kind": "attach",
                "pid": 1234
            },
            "timeout_ms": 1000,
            "ttd_dir": "C:\\TTD"
        });

        let error = decode_arguments::<RecordTtd>(value).expect_err("reject unknown field");

        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("ttd_dir"));
    }
}
