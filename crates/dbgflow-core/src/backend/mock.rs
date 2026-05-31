use super::{
    BackendCapability, BackendInfo, BackendKind, BackendSession, CreateBackendSession, DebugBackend,
};
use crate::{DbgFlowError, Result};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct MockBackend {
    next_id: AtomicU64,
    open_sessions: Mutex<HashSet<String>>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DebugBackend for MockBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "mock".to_string(),
            kind: BackendKind::Mock,
            capabilities: vec![BackendCapability::SessionLifecycle],
        }
    }

    fn create_session(&self, _request: CreateBackendSession) -> Result<BackendSession> {
        let id = format!("mock-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut open_sessions = self
            .open_sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("mock backend lock poisoned".to_string()))?;
        open_sessions.insert(id.clone());
        Ok(BackendSession { id })
    }

    fn close_session(&self, backend_session_id: &str) -> Result<()> {
        let mut open_sessions = self
            .open_sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("mock backend lock poisoned".to_string()))?;

        if open_sessions.remove(backend_session_id) {
            Ok(())
        } else {
            Err(DbgFlowError::Backend(format!(
                "mock session not found: {backend_session_id}"
            )))
        }
    }
}
