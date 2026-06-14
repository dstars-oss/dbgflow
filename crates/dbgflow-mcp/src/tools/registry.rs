use super::adapters::{decode_arguments, to_value};
use dbgflow_common::process::ToolCallContext;
use dbgflow_common::{DbgFlowError, Result};
use dbgflow_debug::backend::DebugTarget;
use dbgflow_debug::session::{EvalSessionResult, Session, SessionId, SessionManager};
use dbgflow_reverse::ida::{
    CreateIdaSession, IdaSessionManager, IdaToolCallResult, ReverseSession, UpstreamIdaToolRequest,
    UpstreamToolDescriptor,
};
use dbgflow_trace::profile::{ProfileCollectorConfig, ProfileManager, ProfileResult, RunProfile};
use dbgflow_trace::ttd::{RecordTtd, TtdRecordingManager, TtdRecordingResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
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
pub const IDA_CREATE_SESSION: &str = "ida.create_session";
pub const IDA_GET_SESSION: &str = "ida.get_session";
pub const IDA_LIST_SESSIONS: &str = "ida.list_sessions";
pub const IDA_CLOSE_SESSION: &str = "ida.close_session";
const IDA_PREFIX: &str = "ida.";

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
    ida_sessions: IdaSessionManager,
}

impl ToolService {
    pub fn new(sessions: SessionManager) -> Self {
        Self {
            sessions,
            profiles: ProfileManager::new("artifacts"),
            ttd_recordings: TtdRecordingManager::new("artifacts"),
            ida_sessions: IdaSessionManager::new("artifacts", Default::default()),
        }
    }

    pub fn with_profiles(sessions: SessionManager, profiles: ProfileManager) -> Self {
        Self {
            sessions,
            profiles,
            ttd_recordings: TtdRecordingManager::new("artifacts"),
            ida_sessions: IdaSessionManager::new("artifacts", Default::default()),
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
            ida_sessions: IdaSessionManager::new("artifacts", Default::default()),
        }
    }

    pub fn with_profiles_ttd_and_reverse(
        sessions: SessionManager,
        profiles: ProfileManager,
        ttd_recordings: TtdRecordingManager,
        ida_sessions: IdaSessionManager,
    ) -> Self {
        Self {
            sessions,
            profiles,
            ttd_recordings,
            ida_sessions,
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
            ida_sessions: IdaSessionManager::new(&root, Default::default()),
        }
    }

    pub fn tool_descriptors(&self) -> Vec<ToolDescriptor> {
        super::schema::tool_descriptors()
    }

    pub fn tool_descriptor_values(&self) -> Vec<Value> {
        let mut tools = self
            .tool_descriptors()
            .into_iter()
            .filter_map(|descriptor| serde_json::to_value(descriptor).ok())
            .collect::<Vec<_>>();
        tools.extend(
            self.ida_sessions
                .upstream_tool_descriptors()
                .into_iter()
                .map(upstream_tool_descriptor_value),
        );
        tools
    }

    pub fn create_session(&self, request: CreateSessionRequest) -> Result<Session> {
        self.create_session_with_context(request, ToolCallContext::default())
    }

    pub fn create_session_with_context(
        &self,
        request: CreateSessionRequest,
        context: ToolCallContext,
    ) -> Result<Session> {
        super::debug::create_session(&self.sessions, request, context)
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
        self.run_profile_with_context(request, ToolCallContext::default())
    }

    pub fn run_profile_with_context(
        &self,
        request: RunProfileRequest,
        context: ToolCallContext,
    ) -> Result<ProfileResult> {
        super::trace::run_profile(&self.profiles, request, context)
    }

    pub fn record_ttd(&self, request: RecordTtd) -> Result<TtdRecordingResult> {
        self.record_ttd_with_context(request, ToolCallContext::default())
    }

    pub fn record_ttd_with_context(
        &self,
        request: RecordTtd,
        context: ToolCallContext,
    ) -> Result<TtdRecordingResult> {
        super::trace::record_ttd(&self.ttd_recordings, request, context)
    }

    pub fn create_ida_session(&self, request: CreateIdaSession) -> Result<ReverseSession> {
        self.create_ida_session_with_context(request, ToolCallContext::default())
    }

    pub fn create_ida_session_with_context(
        &self,
        request: CreateIdaSession,
        context: ToolCallContext,
    ) -> Result<ReverseSession> {
        self.ida_sessions
            .create_session_with_context(request, context)
    }

    pub fn get_ida_session(&self, session_id: SessionId) -> Result<ReverseSession> {
        self.ida_sessions.get_session(session_id)
    }

    pub fn list_ida_sessions(&self) -> Result<Vec<ReverseSession>> {
        self.ida_sessions.list_sessions()
    }

    pub fn close_ida_session(&self, session_id: SessionId, save: bool) -> Result<ReverseSession> {
        self.ida_sessions.close_session_with_save(session_id, save)
    }

    pub fn call_ida_upstream_tool(
        &self,
        tool_name: &str,
        request: UpstreamIdaToolRequest,
    ) -> Result<IdaToolCallResult> {
        self.ida_sessions.call_upstream_tool(tool_name, request)
    }

    pub fn subscribe_session_updates(&self) -> mpsc::Receiver<SessionId> {
        self.sessions.subscribe_session_updates()
    }

    pub fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<Value, ToolCallError> {
        self.call_tool_with_context(name, arguments, ToolCallContext::default())
    }

    pub fn call_tool_with_context(
        &self,
        name: &str,
        arguments: Value,
        context: ToolCallContext,
    ) -> std::result::Result<Value, ToolCallError> {
        match name {
            CREATE_SESSION => self
                .create_session_with_context(decode_arguments(arguments)?, context)
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
                .run_profile_with_context(decode_arguments(arguments)?, context)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            RECORD_TTD => self
                .record_ttd_with_context(decode_arguments(arguments)?, context)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            IDA_CREATE_SESSION => self
                .create_ida_session_with_context(decode_arguments(arguments)?, context)
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            IDA_GET_SESSION => {
                let request: GetIdaSessionRequest = decode_arguments(arguments)?;
                self.get_ida_session(request.session_id)
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_SESSIONS => self
                .list_ida_sessions()
                .map_err(ToolCallError::execution)
                .and_then(to_value),
            IDA_CLOSE_SESSION => {
                let request: CloseIdaSessionRequest = decode_arguments(arguments)?;
                self.close_ida_session(request.session_id, request.save)
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            _ if name.starts_with(IDA_PREFIX) => {
                let upstream_name = &name[IDA_PREFIX.len()..];
                self.call_ida_upstream_tool(upstream_name, decode_arguments(arguments)?)
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
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
pub struct GetIdaSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseIdaSessionRequest {
    pub session_id: SessionId,
    #[serde(default = "default_close_ida_save")]
    pub save: bool,
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

fn default_close_ida_save() -> bool {
    true
}

fn upstream_tool_descriptor_value(tool: UpstreamToolDescriptor) -> Value {
    let mut input_schema = tool.input_schema;
    inject_session_id_schema(&mut input_schema);
    let mut descriptor = Map::new();
    descriptor.insert(
        "name".to_string(),
        Value::String(format!("ida.{}", tool.name)),
    );
    descriptor.insert("description".to_string(), Value::String(tool.description));
    descriptor.insert("inputSchema".to_string(), input_schema);
    if let Some(output_schema) = tool.output_schema {
        descriptor.insert("outputSchema".to_string(), output_schema);
    }
    Value::Object(descriptor)
}

fn inject_session_id_schema(schema: &mut Value) {
    if !schema.is_object() {
        *schema = json!({"type": "object"});
    }
    let object = schema.as_object_mut().expect("object schema");
    object.insert("type".to_string(), Value::String("object".to_string()));
    let properties = object
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if !properties.is_object() {
        *properties = Value::Object(Map::new());
    }
    let props = properties.as_object_mut().expect("properties");
    props.remove("database");
    props.insert(
        "session_id".to_string(),
        json!({
            "type": "string",
            "description": "dbgflow IDA session id returned by ida.create_session."
        }),
    );
    let required = object
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !required.is_array() {
        *required = Value::Array(Vec::new());
    }
    let req = required.as_array_mut().expect("required");
    req.retain(|item| item.as_str() != Some("database"));
    if !req.iter().any(|item| item.as_str() == Some("session_id")) {
        req.push(Value::String("session_id".to_string()));
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
    fn tool_descriptors_include_ida_management_and_upstream_surface() {
        let service = ToolService::new_for_tests();
        let tools = service.tool_descriptor_values();

        assert!(tools.iter().any(|tool| tool["name"] == IDA_CREATE_SESSION));
        assert!(tools.iter().any(|tool| tool["name"] == "ida.decompile"));
        assert!(tools.iter().any(|tool| tool["name"] == "ida.py_eval"));
        assert!(tools.iter().any(|tool| tool["name"] == "ida.idb_save"));
        assert!(!tools.iter().any(|tool| tool["name"] == "ida.idb_open"));
        assert!(!tools.iter().any(|tool| tool["name"] == "ida.idb_list"));
        assert!(!tools.iter().any(|tool| tool["name"] == "ida.py_exec_file"));
        assert!(!tools.iter().any(|tool| tool["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("ida.dbg_"))));

        let decompile = tools
            .iter()
            .find(|tool| tool["name"] == "ida.decompile")
            .expect("decompile");
        assert!(decompile["inputSchema"]["properties"]
            .get("session_id")
            .is_some());
        assert!(decompile["inputSchema"]["properties"]
            .get("database")
            .is_none());
    }

    #[test]
    fn ida_create_session_arguments_decode_upstream_options() {
        let value = json!({
            "target": {
                "kind": "binary",
                "path": "C:\\samples\\a.exe"
            },
            "run_auto_analysis": false,
            "build_caches": false,
            "init_hexrays": true,
            "mode": "force_headless",
            "idle_ttl_sec": 7200,
            "startup_timeout_ms": 60000
        });

        let request: CreateIdaSession = decode_arguments(value).expect("decode ida create");

        assert!(!request.run_auto_analysis);
        assert!(!request.build_caches);
        assert!(request.init_hexrays);
        assert_eq!(request.idle_ttl_sec, 7200);
        assert_eq!(request.startup_timeout_ms, Some(60000));
    }

    #[test]
    fn ida_close_session_defaults_to_save() {
        let value = json!({
            "session_id": "00000000-0000-0000-0000-000000000000"
        });

        let request: CloseIdaSessionRequest = decode_arguments(value).expect("decode close");

        assert!(request.save);
    }

    #[test]
    fn upstream_ida_tool_request_keeps_extra_arguments() {
        let value = json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "addr": "main",
            "count": 10
        });

        let request: UpstreamIdaToolRequest = decode_arguments(value).expect("decode upstream");

        assert_eq!(request.arguments["addr"], "main");
        assert_eq!(request.arguments["count"], 10);
    }

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

        let request = decode_arguments::<RunProfileRequest>(value).expect("decode request");
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

        let request = decode_arguments::<RunProfileRequest>(value).expect("decode request");
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
}
