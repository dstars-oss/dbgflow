use crate::profile::ProfileId;
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

    pub fn ensure_profile_dir(&self, profile_id: ProfileId) -> Result<PathBuf> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        self.ensure_profile_dir_unlocked(profile_id)
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

    pub fn initialize_profile_artifacts(&self, profile_id: ProfileId) -> Result<PathBuf> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let dir = self.ensure_profile_dir_unlocked(profile_id)?;
        fs::create_dir_all(dir.join("target"))
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        fs::create_dir_all(dir.join("collectors"))
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        touch(&dir.join("events.jsonl"))?;
        Ok(dir)
    }

    pub fn command_output_path(&self, session_id: SessionId, command_id: &str) -> PathBuf {
        self.root
            .join("sessions")
            .join(session_id.to_string())
            .join("outputs")
            .join(format!("{command_id}.txt"))
    }

    pub fn profile_trace_path(&self, profile_id: ProfileId) -> PathBuf {
        self.root
            .join("profiles")
            .join(profile_id.to_string())
            .join("trace.etl")
    }

    pub fn profile_metadata_path(&self, profile_id: ProfileId) -> PathBuf {
        self.root
            .join("profiles")
            .join(profile_id.to_string())
            .join("profile.json")
    }

    pub fn profile_stdout_path(&self, profile_id: ProfileId) -> PathBuf {
        self.root
            .join("profiles")
            .join(profile_id.to_string())
            .join("target")
            .join("stdout.txt")
    }

    pub fn profile_stderr_path(&self, profile_id: ProfileId) -> PathBuf {
        self.root
            .join("profiles")
            .join(profile_id.to_string())
            .join("target")
            .join("stderr.txt")
    }

    pub fn profile_collector_dir(
        &self,
        profile_id: ProfileId,
        collector_name: &str,
    ) -> Result<PathBuf> {
        if collector_name.is_empty()
            || collector_name
                .chars()
                .any(|ch| matches!(ch, '/' | '\\') || ch.is_control())
        {
            return Err(DbgFlowError::Artifact(
                "profile collector artifact name is invalid".to_string(),
            ));
        }
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let dir = self
            .ensure_profile_dir_unlocked(profile_id)?
            .join("collectors")
            .join(collector_name);
        fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(dir)
    }

    pub fn profile_collector_artifact_path(
        &self,
        profile_id: ProfileId,
        collector_name: &str,
        file_name: &str,
    ) -> PathBuf {
        self.root
            .join("profiles")
            .join(profile_id.to_string())
            .join("collectors")
            .join(collector_name)
            .join(file_name)
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

    pub fn append_profile_event(
        &self,
        profile_id: ProfileId,
        event: &ProfileArtifactEvent,
    ) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let profile_dir = self.ensure_profile_dir_unlocked(profile_id)?;
        let line = serde_json::to_string(event)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        append_jsonl(&profile_dir.join("events.jsonl"), &line)
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

    fn ensure_profile_dir_unlocked(&self, profile_id: ProfileId) -> Result<PathBuf> {
        let dir = self.root.join("profiles").join(profile_id.to_string());
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

    pub fn write_profile_metadata(
        &self,
        profile_id: ProfileId,
        metadata: &Value,
    ) -> Result<ArtifactRef> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
        let profile_dir = self.ensure_profile_dir_unlocked(profile_id)?;
        let metadata_path = profile_dir.join("profile.json");
        let text = serde_json::to_string_pretty(metadata)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        fs::write(&metadata_path, text)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(ArtifactRef {
            kind: ArtifactKind::ProfileMetadata,
            path: metadata_path,
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
    ProfileTrace,
    ProfileMetadata,
    ProfileEvents,
    ProfileStdout,
    ProfileStderr,
    ProfileCollectorTrace,
    ProfileCollectorSummary,
    ProfileCollectorEvents,
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
pub struct ProfileArtifactEvent {
    pub timestamp_unix_ms: u128,
    pub event: String,
    pub profile_id: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ProfileId;
    use serde_json::json;

    #[test]
    fn profile_artifacts_are_initialized_under_profiles_directory() {
        let root =
            std::env::temp_dir().join(format!("dbgflow-profile-artifacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let artifacts = ArtifactManager::new(&root);
        let profile_id = ProfileId::new();

        let dir = artifacts
            .initialize_profile_artifacts(profile_id)
            .expect("initialize profile artifacts");

        assert_eq!(dir, root.join("profiles").join(profile_id.to_string()));
        assert!(dir.join("events.jsonl").is_file());
        assert!(dir.join("target").is_dir());
        assert!(dir.join("collectors").is_dir());
        assert_eq!(
            artifacts.profile_trace_path(profile_id),
            dir.join("trace.etl")
        );
        assert_eq!(
            artifacts.profile_metadata_path(profile_id),
            dir.join("profile.json")
        );
    }

    #[test]
    fn profile_collector_artifacts_are_under_named_collector_directories() {
        let root = std::env::temp_dir().join(format!(
            "dbgflow-profile-collector-artifacts-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let artifacts = ArtifactManager::new(&root);
        let profile_id = ProfileId::new();

        let dir = artifacts
            .profile_collector_dir(profile_id, "procmon")
            .expect("collector dir");

        assert_eq!(
            dir,
            root.join("profiles")
                .join(profile_id.to_string())
                .join("collectors")
                .join("procmon")
        );
        assert!(dir.is_dir());
        assert_eq!(
            artifacts.profile_collector_artifact_path(profile_id, "procmon", "capture.pml"),
            dir.join("capture.pml")
        );
    }

    #[test]
    fn profile_event_and_metadata_are_written() {
        let root =
            std::env::temp_dir().join(format!("dbgflow-profile-event-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let artifacts = ArtifactManager::new(&root);
        let profile_id = ProfileId::new();
        artifacts
            .initialize_profile_artifacts(profile_id)
            .expect("initialize profile artifacts");

        artifacts
            .append_profile_event(
                profile_id,
                &ProfileArtifactEvent {
                    timestamp_unix_ms: 1,
                    event: "profile_created".to_string(),
                    profile_id: profile_id.to_string(),
                    artifact_path: None,
                    error: None,
                    fields: Map::new(),
                },
            )
            .expect("append profile event");
        artifacts
            .write_profile_metadata(profile_id, &json!({"status": "completed"}))
            .expect("write metadata");

        let dir = root.join("profiles").join(profile_id.to_string());
        let events = std::fs::read_to_string(dir.join("events.jsonl")).expect("read events");
        assert!(events.contains("profile_created"));
        let metadata = std::fs::read_to_string(dir.join("profile.json")).expect("read metadata");
        assert!(metadata.contains("completed"));
    }
}
