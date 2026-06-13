use serde::{Deserialize, Serialize};

pub use dbgflow_common::SessionId;

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
