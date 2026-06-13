use super::adapters::{decode_arguments, to_value};
use dbgflow_common::{DbgFlowError, Result};
use dbgflow_debug::backend::DebugTarget;
use dbgflow_debug::session::{EvalSessionResult, Session, SessionId, SessionManager};
use dbgflow_trace::profile::{ProfileCollectorConfig, ProfileManager, ProfileResult, RunProfile};
use dbgflow_trace::ttd::{RecordTtd, TtdRecordingManager, TtdRecordingResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
        super::schema::tool_descriptors()
    }

    pub fn create_session(&self, request: CreateSessionRequest) -> Result<Session> {
        super::debug::create_session(&self.sessions, request)
    }

    pub fn query_session(&self, session_id: SessionId) -> Result<Session> {
        super::debug::query_session(&self.sessions, session_id)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        super::debug::list_sessions(&self.sessions)
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<Session> {
        super::debug::close_session(&self.sessions, session_id)
    }

    pub fn eval(&self, request: EvalRequest) -> Result<EvalSessionResult> {
        super::debug::eval(&self.sessions, request)
    }

    pub fn add_symbols(&self, request: AddSymbolsRequest) -> Result<EvalSessionResult> {
        super::debug::add_symbols(&self.sessions, request)
    }

    pub fn run_profile(&self, request: RunProfileRequest) -> Result<ProfileResult> {
        super::trace::run_profile(&self.profiles, request)
    }

    pub fn record_ttd(&self, request: RecordTtd) -> Result<TtdRecordingResult> {
        super::trace::record_ttd(&self.ttd_recordings, request)
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
    pub(super) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    pub(super) fn execution(error: DbgFlowError) -> Self {
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
    pub target: dbgflow_trace::profile::ProfileTarget,
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
    target: dbgflow_trace::profile::ProfileTarget,
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
    use dbgflow_trace::profile::{
        EtwEventSet, EtwProfileScope, EtwStackConfig, ProfileCollectorKind,
    };
    use dbgflow_trace::ttd::{
        RecordTtd, TtdRecordMode, TtdRecordingOptions, TtdReplayCpuSupport, TtdTarget,
    };
    use serde_json::json;

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
        let collector_schema = &record_profile.input_schema["properties"]["collectors"]["items"];
        assert_eq!(
            collector_schema["properties"]["kind"]["const"],
            Value::String("native_etw".to_string())
        );
        let event_set_enum = collector_schema["properties"]["event_sets"]["items"]["enum"]
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
    fn run_profile_arguments_reject_procmon_collector() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe"
            },
            "timeout_ms": 1000,
            "collectors": [
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

        let error =
            decode_arguments::<RunProfileRequest>(value).expect_err("reject procmon collector");
        assert!(error.to_string().contains("procmon"));
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
