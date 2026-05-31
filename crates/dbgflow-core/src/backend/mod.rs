pub mod mock;

use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub name: String,
    pub kind: BackendKind,
    pub capabilities: Vec<BackendCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendKind {
    Mock,
    DbgEng,
    Ttd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCapability {
    SessionLifecycle,
    Execute,
    DumpAnalysis,
    LaunchProcess,
    AttachProcess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSession {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateBackendSession {
    pub target: DebugTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugTarget {
    Mock,
}

pub trait DebugBackend: Send + Sync {
    fn info(&self) -> BackendInfo;
    fn create_session(&self, request: CreateBackendSession) -> Result<BackendSession>;
    fn close_session(&self, backend_session_id: &str) -> Result<()>;
}
