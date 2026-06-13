use super::manager::Session;
use super::operation::{operation_status_label, OperationSummary};
use super::SessionState;
use dbgflow_common::artifacts::SessionArtifactEvent;
use serde_json::{Map, Value};
use std::path::PathBuf;

pub(super) fn session_event(
    event: impl Into<String>,
    session: &Session,
    previous_state: Option<SessionState>,
    operation: Option<String>,
    command_id: Option<String>,
    artifact_path: Option<PathBuf>,
    error: Option<String>,
    fields: Map<String, Value>,
) -> SessionArtifactEvent {
    SessionArtifactEvent {
        timestamp_unix_ms: dbgflow_common::time::now_unix_ms(),
        event: event.into(),
        session_id: session.id.to_string(),
        previous_state: previous_state.map(|state| format!("{state:?}")),
        new_state: Some(format!("{:?}", session.state)),
        backend: Some(session.backend.clone()),
        backend_session_id: session.backend_session_id.clone(),
        operation,
        command_id,
        artifact_path,
        error,
        fields,
    }
}

pub(super) fn operation_artifact_event(
    session: &Session,
    previous_state: Option<SessionState>,
    operation: &OperationSummary,
    event: &'static str,
) -> SessionArtifactEvent {
    let mut fields = Map::new();
    fields.insert(
        "status".to_string(),
        Value::String(operation_status_label(operation.status).to_string()),
    );
    if let Some(duration_ms) = operation.duration_ms {
        fields.insert(
            "duration_ms".to_string(),
            Value::Number(serde_json::Number::from(duration_ms as u64)),
        );
    }
    if let Some(output_bytes) = operation.output_bytes {
        fields.insert(
            "output_bytes".to_string(),
            Value::Number(serde_json::Number::from(output_bytes as u64)),
        );
    }
    session_event(
        event,
        session,
        previous_state,
        Some(operation.command.clone()),
        Some(operation.command_id.clone()),
        operation
            .artifact
            .as_ref()
            .map(|artifact| artifact.path.clone()),
        operation.error.clone(),
        fields,
    )
}
