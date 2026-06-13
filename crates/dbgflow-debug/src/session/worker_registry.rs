use super::worker::SessionWorker;
use super::SessionId;
use dbgflow_common::{DbgFlowError, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct WorkerRegistry {
    state: Mutex<WorkerRegistryState>,
}

#[derive(Default)]
struct WorkerRegistryState {
    workers: HashMap<SessionId, Arc<dyn SessionWorker>>,
    cancel_requested: HashSet<SessionId>,
}

impl WorkerRegistry {
    pub(super) fn get(&self, session_id: SessionId) -> Option<Arc<dyn SessionWorker>> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.workers.get(&session_id).cloned())
    }

    pub(super) fn insert(
        &self,
        session_id: SessionId,
        worker: Arc<dyn SessionWorker>,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DbgFlowError::Backend("session worker map poisoned".to_string()))?;
        state.cancel_requested.remove(&session_id);
        state.workers.insert(session_id, worker);
        Ok(())
    }

    pub(super) fn remove(&self, session_id: SessionId) -> Option<Arc<dyn SessionWorker>> {
        self.state.lock().ok().and_then(|mut state| {
            state.cancel_requested.remove(&session_id);
            state.workers.remove(&session_id)
        })
    }

    pub(super) fn kill_once(&self, session_id: SessionId, reason: &str) -> Option<Result<()>> {
        let worker = {
            let mut state = self.state.lock().ok()?;
            if state.cancel_requested.contains(&session_id) {
                return None;
            }
            let worker = state.workers.get(&session_id).cloned()?;
            state.cancel_requested.insert(session_id);
            worker
        };
        Some(worker.kill(reason))
    }

    pub(super) fn is_cancel_requested(&self, session_id: SessionId) -> bool {
        self.state
            .lock()
            .map(|state| state.cancel_requested.contains(&session_id))
            .unwrap_or(false)
    }
}

impl Drop for WorkerRegistry {
    fn drop(&mut self) {
        let state = self.state.get_mut().map(std::mem::take).unwrap_or_default();
        for (session_id, worker) in state.workers {
            let _ = worker.kill(&format!("session_manager_drop:{session_id}"));
        }
    }
}
