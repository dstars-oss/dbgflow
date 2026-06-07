#[cfg(windows)]
pub mod dbgeng;

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
    pub final_state: Option<BackendExecutionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendExecutionState {
    Break,
    Running,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendExecutionEvent {
    pub state: BackendExecutionState,
    pub reason: Option<String>,
}

pub trait BackendEventSink: Send + Sync {
    fn execution_state_changed(&self, event: BackendExecutionEvent);
}

#[derive(Debug, Default)]
pub struct NoopBackendEventSink;

impl BackendEventSink for NoopBackendEventSink {
    fn execution_state_changed(&self, _event: BackendExecutionEvent) {}
}

pub trait DebugBackend: Send + Sync {
    fn info(&self) -> BackendInfo;
    fn create_session(&self, request: CreateBackendSession) -> Result<BackendSession>;
    fn execute(
        &self,
        request: ExecuteBackendRequest,
        event_sink: std::sync::Arc<dyn BackendEventSink>,
    ) -> Result<ExecuteBackendResult>;
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
