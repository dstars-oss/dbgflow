use super::adapters::{decode_arguments, to_value};
use dbgflow_common::process::ToolCallContext;
use dbgflow_common::{DbgFlowError, Result};
use dbgflow_debug::backend::DebugTarget;
use dbgflow_debug::session::{EvalSessionResult, Session, SessionId, SessionManager};
use dbgflow_reverse::ida::{
    CommentItem, CommentView, CreateIdaSession, DecompileRequest, DecompileSessionResult,
    DisassembleRequest, DisassembleResult, IdaRuntimeConfig, IdaSessionManager, ListExportsResult,
    ListFunctionsResult, ListImportsResult, ListSegmentsResult, ListStringsResult,
    ListXrefsRequest, ListXrefsResult, LookupFunctionsRequest, LookupFunctionsResult,
    MetadataResult, MutationResult, PageRequest, RenameItem, RenameRequest, ReverseSession,
    SetCommentRequest, SetTypeRequest, TypeItem, XrefDirection, XrefKind,
};
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
pub const IDA_CREATE_SESSION: &str = "ida.create_session";
pub const IDA_GET_SESSION: &str = "ida.get_session";
pub const IDA_LIST_SESSIONS: &str = "ida.list_sessions";
pub const IDA_CLOSE_SESSION: &str = "ida.close_session";
pub const IDA_GET_METADATA: &str = "ida.get_metadata";
pub const IDA_LIST_SEGMENTS: &str = "ida.list_segments";
pub const IDA_LIST_FUNCTIONS: &str = "ida.list_functions";
pub const IDA_LIST_STRINGS: &str = "ida.list_strings";
pub const IDA_LIST_IMPORTS: &str = "ida.list_imports";
pub const IDA_LIST_EXPORTS: &str = "ida.list_exports";
pub const IDA_LOOKUP_FUNCTIONS: &str = "ida.lookup_functions";
pub const IDA_DISASSEMBLE: &str = "ida.disassemble";
pub const IDA_DECOMPILE: &str = "ida.decompile";
pub const IDA_LIST_XREFS: &str = "ida.list_xrefs";
pub const IDA_RENAME: &str = "ida.rename";
pub const IDA_SET_COMMENT: &str = "ida.set_comment";
pub const IDA_SET_TYPE: &str = "ida.set_type";

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
            ida_sessions: IdaSessionManager::new("artifacts", IdaRuntimeConfig::default()),
        }
    }

    pub fn with_profiles(sessions: SessionManager, profiles: ProfileManager) -> Self {
        Self {
            sessions,
            profiles,
            ttd_recordings: TtdRecordingManager::new("artifacts"),
            ida_sessions: IdaSessionManager::new("artifacts", IdaRuntimeConfig::default()),
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
            ida_sessions: IdaSessionManager::new("artifacts", IdaRuntimeConfig::default()),
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
            ida_sessions: IdaSessionManager::new(&root, IdaRuntimeConfig::default()),
        }
    }

    pub fn tool_descriptors(&self) -> Vec<ToolDescriptor> {
        super::schema::tool_descriptors()
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

    pub fn get_ida_metadata(&self, session_id: SessionId) -> Result<MetadataResult> {
        self.ida_sessions.get_metadata(session_id)
    }

    pub fn list_ida_segments(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<ListSegmentsResult> {
        self.ida_sessions.list_segments_page(session_id, page)
    }

    pub fn list_ida_functions(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<ListFunctionsResult> {
        self.ida_sessions.list_functions_page(session_id, page)
    }

    pub fn list_ida_strings(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<ListStringsResult> {
        self.ida_sessions.list_strings(session_id, page)
    }

    pub fn list_ida_imports(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<ListImportsResult> {
        self.ida_sessions.list_imports(session_id, page)
    }

    pub fn list_ida_exports(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<ListExportsResult> {
        self.ida_sessions.list_exports(session_id, page)
    }

    pub fn lookup_ida_functions(
        &self,
        session_id: SessionId,
        request: LookupFunctionsRequest,
    ) -> Result<LookupFunctionsResult> {
        self.ida_sessions.lookup_functions(session_id, request)
    }

    pub fn disassemble_ida(
        &self,
        session_id: SessionId,
        request: DisassembleRequest,
    ) -> Result<DisassembleResult> {
        self.ida_sessions.disassemble(session_id, request)
    }

    pub fn decompile_ida(
        &self,
        session_id: SessionId,
        request: DecompileRequest,
    ) -> Result<DecompileSessionResult> {
        self.ida_sessions.decompile(session_id, request)
    }

    pub fn list_ida_xrefs(
        &self,
        session_id: SessionId,
        request: ListXrefsRequest,
    ) -> Result<ListXrefsResult> {
        self.ida_sessions.list_xrefs(session_id, request)
    }

    pub fn rename_ida(
        &self,
        session_id: SessionId,
        request: RenameRequest,
    ) -> Result<MutationResult> {
        self.ida_sessions.rename(session_id, request)
    }

    pub fn set_ida_comment(
        &self,
        session_id: SessionId,
        request: SetCommentRequest,
    ) -> Result<MutationResult> {
        self.ida_sessions.set_comment(session_id, request)
    }

    pub fn set_ida_type(
        &self,
        session_id: SessionId,
        request: SetTypeRequest,
    ) -> Result<MutationResult> {
        self.ida_sessions.set_type(session_id, request)
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
            IDA_GET_METADATA => {
                let request: GetIdaSessionRequest = decode_arguments(arguments)?;
                self.get_ida_metadata(request.session_id)
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_SEGMENTS => {
                let request: ListIdaPagedRequest = decode_arguments(arguments)?;
                self.list_ida_segments(request.session_id, request.page())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_FUNCTIONS => {
                let request: ListIdaPagedRequest = decode_arguments(arguments)?;
                self.list_ida_functions(request.session_id, request.page())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_STRINGS => {
                let request: ListIdaPagedRequest = decode_arguments(arguments)?;
                self.list_ida_strings(request.session_id, request.page())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_IMPORTS => {
                let request: ListIdaPagedRequest = decode_arguments(arguments)?;
                self.list_ida_imports(request.session_id, request.page())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_EXPORTS => {
                let request: ListIdaPagedRequest = decode_arguments(arguments)?;
                self.list_ida_exports(request.session_id, request.page())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LOOKUP_FUNCTIONS => {
                let request: IdaLookupFunctionsToolRequest = decode_arguments(arguments)?;
                self.lookup_ida_functions(request.session_id, request.into_inner())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_DISASSEMBLE => {
                let request: IdaDisassembleToolRequest = decode_arguments(arguments)?;
                self.disassemble_ida(request.session_id, request.into_inner())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_DECOMPILE => {
                let request: IdaDecompileToolRequest = decode_arguments(arguments)?;
                self.decompile_ida(request.session_id, request.into_inner())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_LIST_XREFS => {
                let request: IdaListXrefsToolRequest = decode_arguments(arguments)?;
                self.list_ida_xrefs(request.session_id, request.into_inner())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_RENAME => {
                let request: IdaRenameToolRequest = decode_arguments(arguments)?;
                self.rename_ida(request.session_id, request.into_inner())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_SET_COMMENT => {
                let request: IdaSetCommentToolRequest = decode_arguments(arguments)?;
                self.set_ida_comment(request.session_id, request.into_inner())
                    .map_err(ToolCallError::execution)
                    .and_then(to_value)
            }
            IDA_SET_TYPE => {
                let request: IdaSetTypeToolRequest = decode_arguments(arguments)?;
                self.set_ida_type(request.session_id, request.into_inner())
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
pub struct ListIdaPagedRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
    pub filter: Option<String>,
}

impl ListIdaPagedRequest {
    fn page(self) -> PageRequest {
        PageRequest {
            offset: self.offset,
            limit: self.limit,
            filter: self.filter,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaLookupFunctionsToolRequest {
    pub session_id: SessionId,
    pub queries: Vec<String>,
}

impl IdaLookupFunctionsToolRequest {
    fn into_inner(self) -> LookupFunctionsRequest {
        LookupFunctionsRequest {
            queries: self.queries,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaDisassembleToolRequest {
    pub session_id: SessionId,
    pub target: String,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

impl IdaDisassembleToolRequest {
    fn into_inner(self) -> DisassembleRequest {
        DisassembleRequest {
            target: self.target,
            offset: self.offset,
            limit: self.limit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaDecompileToolRequest {
    pub session_id: SessionId,
    pub target: String,
    #[serde(default = "default_include_addresses")]
    pub include_addresses: bool,
}

impl IdaDecompileToolRequest {
    fn into_inner(self) -> DecompileRequest {
        DecompileRequest {
            target: self.target,
            include_addresses: self.include_addresses,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaListXrefsToolRequest {
    pub session_id: SessionId,
    pub target: String,
    #[serde(default = "default_xref_direction")]
    pub direction: XrefDirection,
    #[serde(default = "default_xref_kind")]
    pub kind: XrefKind,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

impl IdaListXrefsToolRequest {
    fn into_inner(self) -> ListXrefsRequest {
        ListXrefsRequest {
            target: self.target,
            direction: self.direction,
            kind: self.kind,
            offset: self.offset,
            limit: self.limit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaRenameToolRequest {
    pub session_id: SessionId,
    pub items: Vec<RenameItem>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub allow_overwrite: bool,
}

impl IdaRenameToolRequest {
    fn into_inner(self) -> RenameRequest {
        RenameRequest {
            items: self.items,
            dry_run: self.dry_run,
            allow_overwrite: self.allow_overwrite,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaSetCommentToolRequest {
    pub session_id: SessionId,
    pub items: Vec<CommentItem>,
    #[serde(default)]
    pub repeatable: bool,
    #[serde(default = "default_comment_view")]
    pub view: CommentView,
}

impl IdaSetCommentToolRequest {
    fn into_inner(self) -> SetCommentRequest {
        SetCommentRequest {
            items: self.items,
            repeatable: self.repeatable,
            view: self.view,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdaSetTypeToolRequest {
    pub session_id: SessionId,
    pub items: Vec<TypeItem>,
    #[serde(default)]
    pub dry_run: bool,
}

impl IdaSetTypeToolRequest {
    fn into_inner(self) -> SetTypeRequest {
        SetTypeRequest {
            items: self.items,
            dry_run: self.dry_run,
        }
    }
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

fn default_include_addresses() -> bool {
    true
}

fn default_xref_direction() -> XrefDirection {
    XrefDirection::Both
}

fn default_xref_kind() -> XrefKind {
    XrefKind::Any
}

fn default_comment_view() -> CommentView {
    CommentView::Disassembly
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
    fn tool_descriptors_include_ida_session_tools() {
        let service = ToolService::new_for_tests();

        let descriptors = service.tool_descriptors();

        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor.name == IDA_CREATE_SESSION));
        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor.name == IDA_LIST_SEGMENTS));
        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor.name == IDA_LIST_FUNCTIONS));
        for name in [
            IDA_GET_METADATA,
            IDA_LIST_STRINGS,
            IDA_LIST_IMPORTS,
            IDA_LIST_EXPORTS,
            IDA_LOOKUP_FUNCTIONS,
            IDA_DISASSEMBLE,
            IDA_DECOMPILE,
            IDA_LIST_XREFS,
            IDA_RENAME,
            IDA_SET_COMMENT,
            IDA_SET_TYPE,
        ] {
            assert!(
                descriptors.iter().any(|descriptor| descriptor.name == name),
                "missing descriptor {name}"
            );
        }
        let close = descriptors
            .iter()
            .find(|descriptor| descriptor.name == IDA_CLOSE_SESSION)
            .expect("ida.close_session descriptor");
        assert!(close.input_schema["properties"].get("save").is_some());
        assert!(!descriptors
            .iter()
            .any(|descriptor| descriptor.name == "ida.list_basic_blocks"));
    }

    #[test]
    fn ida_create_session_arguments_decode_binary_target() {
        let value = json!({
            "target": {
                "kind": "binary",
                "path": "C:\\samples\\a.exe"
            },
            "run_auto_analysis": true,
            "startup_timeout_ms": 60000
        });

        let request: CreateIdaSession = decode_arguments(value).expect("decode ida create");

        assert!(matches!(
            request.target,
            dbgflow_reverse::ida::IdaTarget::Binary { .. }
        ));
        assert!(request.run_auto_analysis);
        assert_eq!(request.startup_timeout_ms, Some(60000));
    }

    #[test]
    fn ida_create_session_arguments_reject_unknown_target_kind() {
        let value = json!({
            "target": {
                "kind": "probe",
                "path": "C:\\samples\\a.exe"
            }
        });

        let error =
            decode_arguments::<CreateIdaSession>(value).expect_err("reject unknown ida target");

        assert!(error.to_string().contains("probe"));
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
    fn ida_tool_arguments_decode_rich_requests() {
        let xrefs = json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "target": "main",
            "direction": "to",
            "kind": "code",
            "offset": 10,
            "limit": 20
        });
        let comments = json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "items": [
                { "target": "0x401000", "comment": "entry point" }
            ],
            "repeatable": true,
            "view": "both"
        });

        let xrefs: IdaListXrefsToolRequest = decode_arguments(xrefs).expect("decode xrefs");
        let comments: IdaSetCommentToolRequest =
            decode_arguments(comments).expect("decode comments");

        assert_eq!(xrefs.direction, XrefDirection::To);
        assert_eq!(xrefs.kind, XrefKind::Code);
        assert_eq!(xrefs.offset, 10);
        assert_eq!(comments.view, CommentView::Both);
        assert!(comments.repeatable);
    }

    #[test]
    fn ida_set_comment_defaults_to_disassembly_view() {
        let comments = json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "items": [
                { "target": "0x401000", "comment": "entry point" }
            ]
        });

        let comments: IdaSetCommentToolRequest =
            decode_arguments(comments).expect("decode comments");

        assert_eq!(comments.view, CommentView::Disassembly);
    }

    #[test]
    fn tools_call_lists_ida_sessions_without_ida_runtime() {
        let service = ToolService::new_for_tests();

        let value = service
            .call_tool(IDA_LIST_SESSIONS, json!({}))
            .expect("list ida sessions");

        assert_eq!(value.as_array().expect("sessions array").len(), 0);
    }

    #[test]
    fn tools_call_rejects_removed_ida_basic_blocks_tool() {
        let service = ToolService::new_for_tests();

        let error = service
            .call_tool(
                "ida.list_basic_blocks",
                json!({ "session_id": SessionId::new(), "target": "0x1000" }),
            )
            .expect_err("removed tool is rejected");

        assert!(error.to_string().contains("unknown tool"));
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
