use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(Uuid);

impl ProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl FromStr for ProfileId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TtdRecordingId(Uuid);

impl TtdRecordingId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TtdRecordingId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TtdRecordingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}
