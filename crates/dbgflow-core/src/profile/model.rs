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
        scope: EtwProfileScope,
        event_sets: Vec<EtwEventSet>,
        #[serde(default)]
        stacks: EtwStackConfig,
    },
}

impl ProfileCollectorConfig {
    pub fn kind(&self) -> ProfileCollectorKind {
        match self {
            Self::NativeEtw { .. } => ProfileCollectorKind::NativeEtw,
        }
    }

    pub fn artifact_name(&self) -> &'static str {
        match self {
            Self::NativeEtw { .. } => "native_etw",
        }
    }
}

impl Default for ProfileCollectorConfig {
    fn default() -> Self {
        Self::NativeEtw {
            scope: EtwProfileScope::TargetProcess,
            event_sets: vec![EtwEventSet::Process, EtwEventSet::FileIo],
            stacks: EtwStackConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EtwProfileScope {
    TargetProcess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EtwEventSet {
    Process,
    FileIo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EtwStackConfig {
    pub enabled: bool,
}

impl Default for EtwStackConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorKind {
    NativeEtw,
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
