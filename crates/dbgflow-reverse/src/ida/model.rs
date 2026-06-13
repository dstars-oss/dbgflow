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
    pub perm: String,
    pub bitness: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionInfo {
    pub index: usize,
    pub start_ea: String,
    pub end_ea: String,
    pub flags: String,
}
