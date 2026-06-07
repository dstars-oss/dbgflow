use super::ProfileId;
use crate::artifacts::ArtifactRef;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunProfile {
    pub target: ProfileTarget,
    pub timeout_ms: u64,
    #[serde(default)]
    pub collectors: Vec<ProfileCollectorConfig>,
}

impl RunProfile {
    pub fn with_default_collectors(mut self) -> Self {
        if self.collectors.is_empty() {
            self.collectors.push(ProfileCollectorConfig::default());
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileTarget {
    Launch {
        executable: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileCollectorConfig {
    NativeEtw {
        preset: ProfilePreset,
    },
    Procmon {
        #[serde(default)]
        capture_stacks: bool,
        #[serde(default)]
        filters: ProcmonFilterConfig,
    },
}

impl ProfileCollectorConfig {
    pub fn kind(&self) -> ProfileCollectorKind {
        match self {
            Self::NativeEtw { .. } => ProfileCollectorKind::NativeEtw,
            Self::Procmon { .. } => ProfileCollectorKind::Procmon,
        }
    }

    pub fn artifact_name(&self) -> &'static str {
        match self {
            Self::NativeEtw { .. } => "native_etw",
            Self::Procmon { .. } => "procmon",
        }
    }
}

impl Default for ProfileCollectorConfig {
    fn default() -> Self {
        Self::NativeEtw {
            preset: ProfilePreset::SystemOverview,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcmonFilterConfig {
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorKind {
    NativeEtw,
    Procmon,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<ArtifactRef>,
    pub profile: ArtifactRef,
    pub events: ArtifactRef,
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCollectorResult {
    pub kind: ProfileCollectorKind,
    pub name: String,
    pub status: ProfileCollectorStatus,
    pub artifacts: Vec<ArtifactRef>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
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
    pub collector_results: Vec<ProfileCollectorResult>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
