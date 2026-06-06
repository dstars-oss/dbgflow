use crate::session::SessionId;
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct ArtifactManager {
    root: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl ArtifactManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_session_dir(&self, session_id: SessionId) -> Result<PathBuf> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        self.ensure_session_dir_unlocked(session_id)
    }

    pub fn initialize_session_artifacts(&self, session_id: SessionId) -> Result<PathBuf> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let dir = self.ensure_session_dir_unlocked(session_id)?;
        fs::create_dir_all(dir.join("outputs"))
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        touch(&dir.join("transcript.log"))?;
        touch(&dir.join("events.jsonl"))?;
        touch(&dir.join("commands.jsonl"))?;
        Ok(dir)
    }

    pub fn command_output_path(&self, session_id: SessionId, command_id: &str) -> PathBuf {
        self.root
            .join("sessions")
            .join(session_id.to_string())
            .join("outputs")
            .join(format!("{command_id}.txt"))
    }

    pub fn append_event(&self, session_id: SessionId, event: &SessionArtifactEvent) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let session_dir = self.ensure_session_dir_unlocked(session_id)?;
        let line = serde_json::to_string(event)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        append_jsonl(&session_dir.join("events.jsonl"), &line)
    }

    pub fn append_transcript(&self, session_id: SessionId, text: &str) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let session_dir = self.ensure_session_dir_unlocked(session_id)?;
        append_text(&session_dir.join("transcript.log"), text)
    }

    pub fn append_command_record(
        &self,
        session_id: SessionId,
        record: &CommandArtifactRecord,
    ) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let session_dir = self.ensure_session_dir_unlocked(session_id)?;
        let line = serde_json::to_string(record)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        append_jsonl(&session_dir.join("commands.jsonl"), &line)
    }

    fn ensure_session_dir_unlocked(&self, session_id: SessionId) -> Result<PathBuf> {
        let dir = self.root.join("sessions").join(session_id.to_string());
        fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(dir)
    }

    pub fn write_eval_artifacts(
        &self,
        session_id: SessionId,
        command_id: &str,
        record: &CommandArtifactRecord,
        output: &str,
    ) -> Result<ArtifactRef> {
        let artifact = self.write_eval_output(session_id, command_id, output)?;
        self.append_command_record(session_id, record)?;
        Ok(artifact)
    }

    pub fn write_eval_output(
        &self,
        session_id: SessionId,
        command_id: &str,
        output: &str,
    ) -> Result<ArtifactRef> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let session_dir = self.ensure_session_dir_unlocked(session_id)?;
        let outputs_dir = session_dir.join("outputs");
        fs::create_dir_all(&outputs_dir)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;

        let output_path = self.command_output_path(session_id, command_id);
        fs::write(&output_path, output)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;

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
pub struct SessionArtifactEvent {
    pub timestamp_unix_ms: u128,
    pub event: String,
    pub session_id: String,
    pub previous_state: Option<String>,
    pub new_state: Option<String>,
    pub backend: Option<String>,
    pub backend_session_id: Option<String>,
    pub operation: Option<String>,
    pub command_id: Option<String>,
    pub artifact_path: Option<PathBuf>,
    pub error: Option<String>,
    pub fields: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandArtifactRecord {
    pub command_id: String,
    pub command: String,
    pub status: String,
    pub output_path: Option<PathBuf>,
    pub started_at_unix_ms: u128,
    pub duration_ms: Option<u128>,
    pub output_bytes: Option<usize>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub backend_session_id: Option<String>,
}

fn append_jsonl(path: &Path, line: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    writeln!(file, "{line}").map_err(|error| DbgFlowError::Artifact(error.to_string()))
}

fn append_text(path: &Path, text: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    file.write_all(text.as_bytes())
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))
}

fn touch(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))
}
