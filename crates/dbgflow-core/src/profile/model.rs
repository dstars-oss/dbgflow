use super::ProfileId;
use crate::artifacts::ArtifactRef;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProfile {
    pub target: ProfileTarget,
    pub timeout_ms: u64,
    pub collector: ProfileCollectorConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileTarget {
    Launch {
        executable: PathBuf,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCollectorConfig {
    pub kind: ProfileCollectorKind,
    pub preset: ProfilePreset,
}

impl Default for ProfileCollectorConfig {
    fn default() -> Self {
        Self {
            kind: ProfileCollectorKind::NativeEtw,
            preset: ProfilePreset::SystemOverview,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorKind {
    NativeEtw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfilePreset {
    SystemOverview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    Completed,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCompletionReason {
    TargetExited,
    Timeout,
    TargetLaunchFailed,
    CollectorError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileArtifacts {
    pub trace: ArtifactRef,
    pub profile: ArtifactRef,
    pub events: ArtifactRef,
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileResult {
    pub profile_id: ProfileId,
    pub status: ProfileStatus,
    pub completion_reason: ProfileCompletionReason,
    pub target_pid: Option<u32>,
    pub target_exit_code: Option<i32>,
    pub duration_ms: u128,
    pub artifacts: ProfileArtifacts,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
