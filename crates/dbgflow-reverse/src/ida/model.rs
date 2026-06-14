use crate::ida::IdaTarget;
use dbgflow_common::artifacts::ArtifactRef;
use dbgflow_common::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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
#[serde(rename_all = "snake_case")]
pub enum IdaOpenMode {
    PreferHeadless,
    ForceHeadless,
    PreferGui,
    ForceGui,
}

impl IdaOpenMode {
    pub fn as_upstream(&self) -> &'static str {
        match self {
            Self::PreferHeadless => "prefer_headless",
            Self::ForceHeadless => "force_headless",
            Self::PreferGui => "prefer_gui",
            Self::ForceGui => "force_gui",
        }
    }
}

impl Default for IdaOpenMode {
    fn default() -> Self {
        Self::PreferHeadless
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaUpstreamSession {
    pub database_id: String,
    pub input_path: PathBuf,
    pub filename: String,
    pub backend: Option<String>,
    pub adopted: Option<bool>,
    pub owned: Option<bool>,
    pub pid: Option<u32>,
    pub worker_pid: Option<u32>,
    pub is_active: Option<bool>,
    pub is_analyzing: Option<bool>,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaSessionHealth {
    pub reachable: bool,
    pub detail: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseSession {
    pub id: SessionId,
    pub backend: String,
    pub target: IdaTarget,
    pub state: ReverseSessionState,
    pub database_id: Option<String>,
    pub ida_backend: Option<String>,
    pub adopted: Option<bool>,
    pub owned: Option<bool>,
    pub pid: Option<u32>,
    pub worker_pid: Option<u32>,
    pub is_active: Option<bool>,
    pub last_health: Option<IdaSessionHealth>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub warnings: Vec<String>,
    pub artifacts: Vec<ArtifactRef>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaToolCallResult {
    pub session_id: SessionId,
    pub tool: String,
    pub result: Value,
    pub artifact: ArtifactRef,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseDatabaseResult {
    pub save_requested: bool,
    pub save_status: SaveStatus,
    pub warning: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveStatus {
    NotRequested,
    Saved,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UpstreamIdaToolRequest {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub arguments: Map<String, Value>,
}
