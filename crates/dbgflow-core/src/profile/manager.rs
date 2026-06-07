use super::{
    validate_profile_target, CollectorFactory, ProcessTargetRunner, ProfileArtifacts,
    ProfileCollector, ProfileCollectorResult, ProfileCollectorStatus, ProfileCompletionReason,
    ProfileId, ProfileResult, ProfileStatus, RunProfile, TargetExit, TargetRunner,
};
use crate::artifacts::{ArtifactKind, ArtifactManager, ArtifactRef, ProfileArtifactEvent};
use crate::{DbgFlowError, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct ProfileManager {
    artifacts: ArtifactManager,
    collector_factory: Arc<dyn CollectorFactory>,
    target_runner: Arc<dyn TargetRunner>,
    active_job: Arc<Mutex<Option<ProfileId>>>,
}

impl ProfileManager {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self::with_runtime(artifact_root, super::ProcmonRuntime::unavailable())
    }

    pub fn with_runtime(artifact_root: impl Into<PathBuf>, procmon: super::ProcmonRuntime) -> Self {
        Self::with_components(
            artifact_root,
            Arc::new(super::DefaultProfileCollectorFactory::new(procmon)),
            Arc::new(ProcessTargetRunner),
        )
    }

    pub fn with_components(
        artifact_root: impl Into<PathBuf>,
        collector_factory: Arc<dyn CollectorFactory>,
        target_runner: Arc<dyn TargetRunner>,
    ) -> Self {
        Self {
            artifacts: ArtifactManager::new(artifact_root),
            collector_factory,
            target_runner,
            active_job: Arc::new(Mutex::new(None)),
        }
    }

    pub fn run_profile(&self, mut request: RunProfile) -> Result<ProfileResult> {
        request.target = validate_profile_target(request.target)?;
        request = request.with_default_collectors();
        if request.timeout_ms == 0 {
            return Err(DbgFlowError::Backend(
                "profile timeout_ms must be greater than zero".to_string(),
            ));
        }
        let profile_id = ProfileId::new();
        {
            let mut active = self.active_job.lock().map_err(|_| {
                DbgFlowError::Backend("profile active job lock poisoned".to_string())
            })?;
            if let Some(active_id) = *active {
                return Err(DbgFlowError::Backend(format!(
                    "another profile job is already active: {active_id}"
                )));
            }
            *active = Some(profile_id);
        }
        let active_guard = ActiveProfileGuard {
            active_job: self.active_job.clone(),
            profile_id,
        };

        let started = Instant::now();
        let started_at = now_unix_ms();
        let profile_dir = self.artifacts.initialize_profile_artifacts(profile_id)?;
        self.record_event(profile_id, "profile_created", None, None, Map::new());

        let stdout_path = self.artifacts.profile_stdout_path(profile_id);
        let stderr_path = self.artifacts.profile_stderr_path(profile_id);
        ensure_empty_file(&stdout_path)?;
        ensure_empty_file(&stderr_path)?;

        let events_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileEvents,
            path: profile_dir.join("events.jsonl"),
        };
        let stdout_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileStdout,
            path: stdout_path.clone(),
        };
        let stderr_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileStderr,
            path: stderr_path.clone(),
        };

        let mut warnings = Vec::new();
        let mut started_collectors = Vec::new();
        for config in &request.collectors {
            let collector_dir = self
                .artifacts
                .profile_collector_dir(profile_id, config.artifact_name())?;
            self.record_event(
                profile_id,
                "collector_starting",
                Some(collector_dir.clone()),
                None,
                collector_fields(config),
            );
            let collector = match self.collector_factory.create(config, &collector_dir) {
                Ok(collector) => collector,
                Err(error) => {
                    stop_started_collectors_for_start_failure(
                        self,
                        profile_id,
                        &mut started_collectors,
                    );
                    return Err(error);
                }
            };
            match collector.start() {
                Ok(start) => {
                    warnings.extend(start.warnings);
                    self.record_event(
                        profile_id,
                        "collector_started",
                        Some(collector_dir),
                        None,
                        collector_fields(config),
                    );
                    started_collectors.push(collector);
                }
                Err(error) => {
                    stop_started_collectors_for_start_failure(
                        self,
                        profile_id,
                        &mut started_collectors,
                    );
                    self.record_event(
                        profile_id,
                        "profile_error",
                        None,
                        Some(error.to_string()),
                        collector_fields(config),
                    );
                    return Err(error);
                }
            }
        }

        self.record_event(profile_id, "target_launching", None, None, Map::new());
        let target_exit = self.target_runner.launch_and_wait(
            &request.target,
            Duration::from_millis(request.timeout_ms),
            &stdout_path,
            &stderr_path,
        );

        let mut stop_error = None;
        let mut collector_results = Vec::new();
        for collector in started_collectors.into_iter().rev() {
            let name = collector.name().to_string();
            let kind = collector.kind();
            let mut fields = Map::new();
            fields.insert("collector".to_string(), Value::String(name.clone()));
            self.record_event(profile_id, "collector_stopping", None, None, fields.clone());
            match collector.stop() {
                Ok(stop) => {
                    warnings.extend(stop.warnings.clone());
                    self.record_event(profile_id, "collector_stopped", None, None, fields);
                    collector_results.push(ProfileCollectorResult {
                        kind,
                        name,
                        status: ProfileCollectorStatus::Completed,
                        artifacts: stop.artifacts,
                        warnings: stop.warnings,
                        error: None,
                    });
                }
                Err(error) => {
                    let error = error.to_string();
                    if stop_error.is_none() {
                        stop_error = Some(error.clone());
                    }
                    warnings.push(format!("collector {name} stop failed: {error}"));
                    self.record_event(
                        profile_id,
                        "profile_error",
                        None,
                        Some(error.clone()),
                        fields,
                    );
                    collector_results.push(ProfileCollectorResult {
                        kind,
                        name,
                        status: ProfileCollectorStatus::Failed,
                        artifacts: Vec::new(),
                        warnings: Vec::new(),
                        error: Some(error),
                    });
                }
            }
        }
        collector_results.reverse();
        let trace_artifact = legacy_trace_artifact(&collector_results);

        let duration_ms = started.elapsed().as_millis();
        let (status, completion_reason, target_pid, target_exit_code, error) = match target_exit {
            Ok(TargetExit::Exited { pid, exit_code }) => {
                self.record_target_started(profile_id, pid);
                self.record_event(profile_id, "target_exited", None, None, Map::new());
                (
                    ProfileStatus::Completed,
                    ProfileCompletionReason::TargetExited,
                    Some(pid),
                    exit_code,
                    stop_error,
                )
            }
            Ok(TargetExit::TimedOut { pid }) => {
                self.record_target_started(profile_id, pid);
                self.record_event(profile_id, "timeout_reached", None, None, Map::new());
                (
                    ProfileStatus::TimedOut,
                    ProfileCompletionReason::Timeout,
                    Some(pid),
                    None,
                    stop_error,
                )
            }
            Err(error) => {
                let error = error.to_string();
                self.record_event(
                    profile_id,
                    "profile_error",
                    None,
                    Some(error.clone()),
                    Map::new(),
                );
                (
                    ProfileStatus::Failed,
                    ProfileCompletionReason::TargetLaunchFailed,
                    None,
                    None,
                    Some(stop_error.unwrap_or(error)),
                )
            }
        };

        let metadata_artifact = self.write_metadata(
            profile_id,
            &request,
            status,
            completion_reason,
            target_pid,
            target_exit_code,
            started_at,
            duration_ms,
            trace_artifact.as_ref(),
            &collector_results,
            &warnings,
            error.clone(),
        )?;

        let result = ProfileResult {
            profile_id,
            status,
            completion_reason,
            target_pid,
            target_exit_code,
            duration_ms,
            artifacts: ProfileArtifacts {
                trace: trace_artifact,
                profile: metadata_artifact,
                events: events_artifact,
                stdout: stdout_artifact,
                stderr: stderr_artifact,
            },
            collector_results,
            warnings,
            error,
        };
        self.record_event(profile_id, "profile_completed", None, None, Map::new());
        drop(active_guard);
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_metadata(
        &self,
        profile_id: ProfileId,
        request: &RunProfile,
        status: ProfileStatus,
        completion_reason: ProfileCompletionReason,
        target_pid: Option<u32>,
        target_exit_code: Option<i32>,
        started_at_unix_ms: u128,
        duration_ms: u128,
        trace_artifact: Option<&ArtifactRef>,
        collector_results: &[ProfileCollectorResult],
        warnings: &[String],
        error: Option<String>,
    ) -> Result<ArtifactRef> {
        let metadata = json!({
            "profile_id": profile_id.to_string(),
            "target": request.target,
            "target_pid": target_pid,
            "start_time_unix_ms": started_at_unix_ms,
            "end_time_unix_ms": now_unix_ms(),
            "duration_ms": duration_ms,
            "timeout_ms": request.timeout_ms,
            "status": status,
            "completion_reason": completion_reason,
            "target_exit_code": target_exit_code,
            "collectors": request.collectors,
            "trace": trace_artifact.map(|artifact| artifact.path.clone()),
            "collector_results": collector_results,
            "warnings": warnings,
            "error": error,
        });
        self.artifacts.write_profile_metadata(profile_id, &metadata)
    }

    fn record_target_started(&self, profile_id: ProfileId, pid: u32) {
        let mut fields = Map::new();
        fields.insert(
            "pid".to_string(),
            Value::Number(serde_json::Number::from(pid)),
        );
        self.record_event(profile_id, "target_started", None, None, fields);
    }

    fn record_event(
        &self,
        profile_id: ProfileId,
        event: &str,
        artifact_path: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        let _ = self.artifacts.append_profile_event(
            profile_id,
            &ProfileArtifactEvent {
                timestamp_unix_ms: now_unix_ms(),
                event: event.to_string(),
                profile_id: profile_id.to_string(),
                artifact_path,
                error,
                fields,
            },
        );
    }
}

struct ActiveProfileGuard {
    active_job: Arc<Mutex<Option<ProfileId>>>,
    profile_id: ProfileId,
}

impl Drop for ActiveProfileGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active_job.lock() {
            if *active == Some(self.profile_id) {
                *active = None;
            }
        }
    }
}

fn ensure_empty_file(path: &PathBuf) -> Result<()> {
    fs::write(path, b"").map_err(|error| DbgFlowError::Artifact(error.to_string()))
}

fn collector_fields(config: &super::ProfileCollectorConfig) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "collector".to_string(),
        Value::String(config.artifact_name().to_string()),
    );
    fields
}

fn legacy_trace_artifact(results: &[ProfileCollectorResult]) -> Option<ArtifactRef> {
    results
        .iter()
        .find(|result| result.name == "native_etw")
        .and_then(|result| {
            result
                .artifacts
                .iter()
                .find(|artifact| artifact.kind == ArtifactKind::ProfileCollectorTrace)
        })
        .map(|artifact| ArtifactRef {
            kind: ArtifactKind::ProfileTrace,
            path: artifact.path.clone(),
        })
}

fn stop_started_collectors_for_start_failure(
    manager: &ProfileManager,
    profile_id: ProfileId,
    started_collectors: &mut Vec<Box<dyn ProfileCollector>>,
) {
    while let Some(collector) = started_collectors.pop() {
        let name = collector.name().to_string();
        let mut fields = Map::new();
        fields.insert("collector".to_string(), Value::String(name));
        manager.record_event(profile_id, "collector_stopping", None, None, fields.clone());
        match collector.stop() {
            Ok(_) => manager.record_event(profile_id, "collector_stopped", None, None, fields),
            Err(error) => manager.record_event(
                profile_id,
                "profile_error",
                None,
                Some(error.to_string()),
                fields,
            ),
        }
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
