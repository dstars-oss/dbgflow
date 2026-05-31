use crate::session::SessionId;
use crate::{DbgFlowError, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ArtifactManager {
    root: PathBuf,
}

impl ArtifactManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_session_dir(&self, session_id: SessionId) -> Result<PathBuf> {
        let dir = self.root.join("sessions").join(session_id.to_string());
        fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(dir)
    }
}
