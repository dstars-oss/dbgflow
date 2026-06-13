use crate::ida::IdaTarget;
use dbgflow_common::artifacts::ArtifactRef;
use dbgflow_common::SessionId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReverseSessionState {
    Starting,
    Ready,
    Closing,
    Closed,
    Error,
}

impl ReverseSessionState {
    pub fn is_reusable(&self) -> bool {
        matches!(self, Self::Starting | Self::Ready)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaVersion {
    pub major: i32,
    pub minor: i32,
    pub build: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaInfo {
    pub install_dir: PathBuf,
    pub version: IdaVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseSession {
    pub id: SessionId,
    pub backend: String,
    pub target: IdaTarget,
    pub state: ReverseSessionState,
    pub ida: Option<IdaInfo>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub warnings: Vec<String>,
    pub artifacts: Vec<ArtifactRef>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub index: usize,
    pub start_ea: String,
    pub end_ea: String,
    pub size: String,
    pub name: Option<String>,
    pub class: Option<String>,
    pub perm: String,
    pub bitness: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionInfo {
    pub index: usize,
    pub start_ea: String,
    pub end_ea: String,
    pub size: String,
    pub name: Option<String>,
    pub segment: Option<String>,
    pub prototype: Option<String>,
    pub flags: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
    pub filter: Option<String>,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: Some(100),
            filter: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInfo {
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub returned: usize,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaMetadata {
    pub target: IdaTarget,
    pub ida: Option<IdaInfo>,
    pub segments: usize,
    pub functions: usize,
    pub rich_api: IdaRichApiStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaRichApiStatus {
    pub available: bool,
    pub direct_bindings: bool,
    pub ida_version_gate: String,
    pub capabilities: DirectIdaCapabilities,
    pub missing_symbols: Vec<String>,
    pub hexrays: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectIdaCapabilities {
    pub names: bool,
    pub disassembly: bool,
    pub strings: bool,
    pub imports: bool,
    pub exports: bool,
    pub xrefs: bool,
    pub basic_blocks: bool,
    pub comments: bool,
    pub types: bool,
    pub decompiler: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseDatabaseResult {
    pub save_requested: bool,
    pub save_status: SaveStatus,
    pub warning: Option<String>,
    pub error: Option<String>,
}

impl CloseDatabaseResult {
    pub fn from_idalib_close(save: bool) -> Self {
        if save {
            Self {
                save_requested: true,
                save_status: SaveStatus::Unknown,
                warning: Some(
                    "idalib close_database(save=true) completed, but the IDA C ABI does not report whether saving succeeded".to_string(),
                ),
                error: None,
            }
        } else {
            Self {
                save_requested: false,
                save_status: SaveStatus::NotRequested,
                warning: None,
                error: None,
            }
        }
    }

    pub fn no_worker(save: bool) -> Self {
        Self {
            save_requested: save,
            save_status: if save {
                SaveStatus::Unknown
            } else {
                SaveStatus::NotRequested
            },
            warning: Some(
                "IDA worker was not available during close; no database save result could be observed"
                    .to_string(),
            ),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveStatus {
    NotRequested,
    Saved,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StringInfo {
    pub index: usize,
    pub ea: String,
    pub length: usize,
    pub string_type: Option<String>,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportInfo {
    pub index: usize,
    pub ea: String,
    pub module: Option<String>,
    pub name: Option<String>,
    pub ordinal: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportInfo {
    pub index: usize,
    pub ea: String,
    pub name: Option<String>,
    pub ordinal: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionLookup {
    pub query: String,
    pub function: Option<FunctionInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisassemblyLine {
    pub ea: String,
    pub text: String,
    pub label: Option<String>,
    pub comments: Vec<String>,
    pub refs: Vec<XrefInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disassembly {
    pub target: String,
    pub function: Option<FunctionInfo>,
    pub lines: Vec<DisassemblyLine>,
    pub page: PageInfo,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecompileResult {
    pub target: String,
    pub function: Option<FunctionInfo>,
    pub code: Option<String>,
    pub refs: Vec<XrefInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XrefInfo {
    pub direction: Option<String>,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub type_name: Option<String>,
    pub user: bool,
    pub function: Option<FunctionInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XrefsResult {
    pub target: String,
    pub xrefs: Vec<XrefInfo>,
    pub page: PageInfo,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasicBlockInfo {
    pub id: usize,
    pub start_ea: String,
    pub end_ea: String,
    pub successors: Vec<String>,
    pub predecessors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasicBlocksResult {
    pub target: String,
    pub function: Option<FunctionInfo>,
    pub blocks: Vec<BasicBlockInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupFunctionsRequest {
    pub queries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisassembleRequest {
    pub target: String,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecompileRequest {
    pub target: String,
    #[serde(default = "default_include_addresses")]
    pub include_addresses: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XrefDirection {
    To,
    From,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XrefKind {
    Any,
    Code,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListXrefsRequest {
    pub target: String,
    #[serde(default = "default_xref_direction")]
    pub direction: XrefDirection,
    #[serde(default = "default_xref_kind")]
    pub kind: XrefKind,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasicBlocksRequest {
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameItem {
    pub target: String,
    pub name: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameRequest {
    pub items: Vec<RenameItem>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub allow_overwrite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentItem {
    pub target: String,
    pub comment: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentView {
    Disassembly,
    Decompiler,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCommentRequest {
    pub items: Vec<CommentItem>,
    #[serde(default)]
    pub repeatable: bool,
    #[serde(default = "default_comment_view")]
    pub view: CommentView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeItem {
    pub target: String,
    pub type_text: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTypeRequest {
    pub items: Vec<TypeItem>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationItemResult {
    pub target: String,
    pub old: Option<String>,
    pub new: Option<String>,
    pub success: bool,
    pub dry_run: bool,
    pub error: Option<String>,
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
    CommentView::Both
}
