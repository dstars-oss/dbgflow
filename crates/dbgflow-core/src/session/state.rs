use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Created,
    Starting,
    Ready,
    Break,
    Running,
    Closing,
    Closed,
    Error,
}

impl SessionState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Error)
    }

    pub fn is_reusable(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Starting | Self::Ready | Self::Break | Self::Running
        )
    }
}
