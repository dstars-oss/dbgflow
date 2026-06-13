use dbgflow_common::artifacts::{ArtifactRef, CommandArtifactRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSummary {
    pub command_id: String,
    pub command: String,
    pub status: OperationStatus,
    pub started_at_unix_ms: u128,
    pub finished_at_unix_ms: Option<u128>,
    pub duration_ms: Option<u128>,
    pub artifact: Option<ArtifactRef>,
    pub error: Option<String>,
    pub output_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationStatus {
    Running,
    CancelRequested,
    Canceled,
    Finished,
    Failed,
}

pub(super) fn finalize_canceled_operation(
    session: &mut super::manager::Session,
    status: OperationStatus,
    error: String,
) -> Option<OperationSummary> {
    let now = dbgflow_common::time::now_unix_ms();
    if let Some(operation) = session.last_operation.as_mut() {
        if matches!(
            operation.status,
            OperationStatus::Running | OperationStatus::CancelRequested
        ) {
            operation.status = status;
            operation.finished_at_unix_ms = Some(now);
            operation.duration_ms = Some(now.saturating_sub(operation.started_at_unix_ms));
            operation.error = Some(error);
            return Some(operation.clone());
        }
    }
    None
}

pub(super) fn command_record_from_operation(
    operation: &OperationSummary,
    backend_session_id: Option<String>,
) -> CommandArtifactRecord {
    CommandArtifactRecord {
        command_id: operation.command_id.clone(),
        command: operation.command.clone(),
        status: operation_status_label(operation.status).to_string(),
        output_path: operation
            .artifact
            .as_ref()
            .map(|artifact| artifact.path.clone()),
        started_at_unix_ms: operation.started_at_unix_ms,
        duration_ms: operation.duration_ms,
        output_bytes: operation.output_bytes,
        warnings: Vec::new(),
        error: operation.error.clone(),
        backend_session_id,
    }
}

pub(super) fn operation_status_label(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Running => "Running",
        OperationStatus::CancelRequested => "CancelRequested",
        OperationStatus::Canceled => "Canceled",
        OperationStatus::Finished => "Finished",
        OperationStatus::Failed => "Failed",
    }
}
