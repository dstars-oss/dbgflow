use crate::session::SessionId;
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
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

    pub fn write_execute_artifacts(
        &self,
        session_id: SessionId,
        command_id: &str,
        record: &CommandArtifactRecord,
        output: &str,
    ) -> Result<ArtifactRef> {
        let session_dir = self.ensure_session_dir(session_id)?;
        let outputs_dir = session_dir.join("outputs");
        fs::create_dir_all(&outputs_dir)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;

        let output_path = outputs_dir.join(format!("{command_id}.txt"));
        fs::write(&output_path, output)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;

        let commands_path = session_dir.join("commands.jsonl");
        let line = serde_json::to_string(record)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        append_jsonl(&commands_path, &line)?;

        Ok(ArtifactRef {
            kind: ArtifactKind::CommandOutput,
            path: output_path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub kind: ArtifactKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    CommandOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandArtifactRecord {
    pub command_id: String,
    pub command: String,
    pub output_path: PathBuf,
    pub started_at_unix_ms: u128,
    pub duration_ms: u128,
    pub output_bytes: usize,
    pub output_truncated_in_response: bool,
}

fn append_jsonl(path: &Path, line: &str) -> Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    writeln!(file, "{line}").map_err(|error| DbgFlowError::Artifact(error.to_string()))
}
