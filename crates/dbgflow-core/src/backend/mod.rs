#[cfg(windows)]
pub mod dbgeng;
pub mod mock;

use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateBackendSession {
    pub target: DebugTarget,
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugTarget {
    Mock,
    Dump {
        path: PathBuf,
    },
    Attach {
        pid: u32,
    },
    Launch {
        executable: PathBuf,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteBackendRequest {
    pub backend_session_id: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteBackendResult {
    pub output: String,
    pub warnings: Vec<String>,
}

pub trait DebugBackend: Send + Sync {
    fn info(&self) -> BackendInfo;
    fn create_session(&self, request: CreateBackendSession) -> Result<BackendSession>;
    fn execute(&self, request: ExecuteBackendRequest) -> Result<ExecuteBackendResult>;
    fn cancel_startup(&self, _correlation_id: &str) -> Result<()> {
        Err(DbgFlowError::Backend(
            "backend does not support startup cancellation".to_string(),
        ))
    }
    fn cancel_session(&self, _backend_session_id: &str) -> Result<()> {
        Err(DbgFlowError::Backend(
            "backend does not support session cancellation".to_string(),
        ))
    }
    fn close_session(&self, backend_session_id: &str) -> Result<()>;
}
