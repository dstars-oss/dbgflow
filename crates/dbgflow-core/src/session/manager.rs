use super::{SessionId, SessionState};
use crate::backend::mock::MockBackend;
use crate::backend::{CreateBackendSession, DebugBackend, DebugTarget};
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSession {
    pub target: DebugTarget,
}

impl Default for CreateSession {
    fn default() -> Self {
        Self {
            target: DebugTarget::Mock,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub backend: String,
    pub backend_session_id: String,
    pub target: DebugTarget,
    pub state: SessionState,
    pub created_at_unix_ms: u128,
}

#[derive(Clone)]
pub struct SessionManager {
    backends: HashMap<String, Arc<dyn DebugBackend>>,
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
}

impl SessionManager {
    pub fn new(backends: Vec<Arc<dyn DebugBackend>>) -> Self {
        let backends = backends
            .into_iter()
            .map(|backend| (backend.info().name, backend))
            .collect();

        Self {
            backends,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_mock_backend() -> Self {
        Self::new(vec![Arc::new(MockBackend::new())])
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?
            .values()
            .cloned()
            .collect::<Vec<_>>();

        sessions.sort_by_key(|session| session.created_at_unix_ms);
        Ok(sessions)
    }

    pub fn create_session(&self, request: CreateSession) -> Result<Session> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;

        if let Some(existing) = sessions
            .values()
            .find(|session| session.target == request.target && !session.state.is_terminal())
            .cloned()
        {
            return Ok(existing);
        }

        let backend_name = select_backend_for_target(&request.target);
        let backend = self
            .backends
            .get(&backend_name)
            .ok_or_else(|| DbgFlowError::BackendNotFound(backend_name.clone()))?;

        let backend_session = backend.create_session(CreateBackendSession {
            target: request.target.clone(),
        })?;

        let session = Session {
            id: SessionId::new(),
            backend: backend_name,
            backend_session_id: backend_session.id,
            target: request.target,
            state: SessionState::Ready,
            created_at_unix_ms: now_unix_ms(),
        };

        sessions.insert(session.id, session.clone());

        Ok(session)
    }

    pub fn query_session(&self, session_id: SessionId) -> Result<Session> {
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<Session> {
        let (backend_name, backend_session_id) = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
            let session = sessions
                .get_mut(&session_id)
                .ok_or(DbgFlowError::SessionNotFound(session_id))?;

            if session.state == SessionState::Closed {
                return Err(DbgFlowError::SessionClosed(session_id));
            }

            session.state = SessionState::Closing;
            (session.backend.clone(), session.backend_session_id.clone())
        };

        let backend = self
            .backends
            .get(&backend_name)
            .ok_or_else(|| DbgFlowError::BackendNotFound(backend_name.clone()))?;

        let close_result = backend.close_session(&backend_session_id);

        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;

        match close_result {
            Ok(()) => {
                session.state = SessionState::Closed;
                Ok(session.clone())
            }
            Err(error) => {
                session.state = SessionState::Error;
                Err(error)
            }
        }
    }
}

fn select_backend_for_target(target: &DebugTarget) -> String {
    match target {
        DebugTarget::Mock => "mock".to_string(),
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
