use super::{
    validate_profile_target, CollectorFactory, ProcessTargetRunner, ProfileArtifacts,
    ProfileCollector, ProfileCollectorKind, ProfileCollectorResult, ProfileCollectorStatus,
    ProfileCompletionReason, ProfileId, ProfileResult, ProfileStatus, RunProfile, TargetEventSink,
    TargetExit, TargetRunner,
};
use dbgflow_common::artifacts::{ArtifactKind, ArtifactManager, ArtifactRef, ProfileArtifactEvent};
use dbgflow_common::job::SingleActiveJob;
use dbgflow_common::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use dbgflow_common::process::{ProcessLaunchConfig, ProcessLaunchContext, ToolCallContext};
use dbgflow_common::time::now_unix_ms;
use dbgflow_common::{DbgFlowError, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct ProfileManager {
    artifacts: ArtifactManager,
    collector_factory: Arc<dyn CollectorFactory>,
    target_runner: Arc<dyn TargetRunner>,
    active_job: SingleActiveJob<ProfileId>,
    logger: Arc<dyn LogSink>,
    process_launch: ProcessLaunchConfig,
}

impl ProfileManager {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self::with_logger(artifact_root, noop_logger())
    }

    pub fn with_logger(artifact_root: impl Into<PathBuf>, logger: Arc<dyn LogSink>) -> Self {
        Self::with_logger_and_process_launch(artifact_root, logger, ProcessLaunchConfig::default())
    }

    pub fn with_logger_and_process_launch(
        artifact_root: impl Into<PathBuf>,
        logger: Arc<dyn LogSink>,
        process_launch: ProcessLaunchConfig,
    ) -> Self {
        Self::with_components_and_logger(
            artifact_root,
            Arc::new(super::DefaultProfileCollectorFactory::new()),
            Arc::new(ProcessTargetRunner),
            logger,
            process_launch,
        )
    }

    pub fn with_components(
        artifact_root: impl Into<PathBuf>,
        collector_factory: Arc<dyn CollectorFactory>,
        target_runner: Arc<dyn TargetRunner>,
    ) -> Self {
        Self::with_components_and_logger(
            artifact_root,
            collector_factory,
            target_runner,
            noop_logger(),
            ProcessLaunchConfig::default(),
        )
    }

    pub fn with_components_and_logger(
        artifact_root: impl Into<PathBuf>,
        collector_factory: Arc<dyn CollectorFactory>,
        target_runner: Arc<dyn TargetRunner>,
        logger: Arc<dyn LogSink>,
        process_launch: ProcessLaunchConfig,
    ) -> Self {
        Self {
            artifacts: ArtifactManager::new(artifact_root),
            collector_factory,
            target_runner,
            active_job: SingleActiveJob::default(),
            logger,
            process_launch,
        }
    }

    pub fn run_profile(&self, request: RunProfile) -> Result<ProfileResult> {
        self.run_profile_with_context(request, ToolCallContext::default())
    }

    pub fn run_profile_with_context(
        &self,
        mut request: RunProfile,
        tool_context: ToolCallContext,
    ) -> Result<ProfileResult> {
        let request_started = Instant::now();
        let requested_target = request.target.clone();
        request.target = match validate_profile_target(request.target) {
            Ok(target) => target,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "profile", "run_profile_rejected")
                        .duration_ms(request_started.elapsed().as_millis())
                        .field("target", requested_target)
                        .error(error.to_string()),
                );
                return Err(error);
            }
        };
        request = request.with_default_collectors();
        if request.timeout_ms == 0 {
            let error =
                DbgFlowError::Backend("profile timeout_ms must be greater than zero".to_string());
            self.log_profile_rejected(&request, request_started.elapsed().as_millis(), &error);
            return Err(error);
        }
        if let Err(error) = reject_duplicate_collectors(&request) {
            self.log_profile_rejected(&request, request_started.elapsed().as_millis(), &error);
            return Err(error);
        }

        let profile_id = ProfileId::new();
        let active_guard = match self.active_job.start(profile_id, |active_id| {
            format!("another profile job is already active: {active_id}")
        }) {
            Ok(guard) => guard,
            Err(error) => {
                self.log_profile_rejected(&request, request_started.elapsed().as_millis(), &error);
                return Err(error);
            }
        };

        let started = Instant::now();
        let started_at = now_unix_ms();
        self.log(
            LogEvent::new(LogLevel::Info, "profile", "run_profile_started")
                .field("profile_id", profile_id.to_string())
                .field("target", &request.target)
                .field("timeout_ms", request.timeout_ms)
                .field("collectors", collector_names(&request)),
        );

        let profile_dir = match self.artifacts.initialize_profile_artifacts(profile_id) {
            Ok(profile_dir) => profile_dir,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "profile", "profile_artifacts_init_failed")
                        .field("profile_id", profile_id.to_string())
                        .duration_ms(started.elapsed().as_millis())
                        .error(error.to_string()),
                );
                return Err(error);
            }
        };
        self.record_event(
            profile_id,
            "profile_created",
            None,
            None,
            profile_request_fields(&request),
        );

        let stdout_path = self.artifacts.profile_stdout_path(profile_id);
        let stderr_path = self.artifacts.profile_stderr_path(profile_id);
        if let Err(error) = ensure_empty_file(&stdout_path) {
            self.finish_failed_profile(
                profile_id,
                &request,
                ProfileCompletionReason::CollectorError,
                started_at,
                started.elapsed().as_millis(),
                None,
                &[],
                &[],
                error.to_string(),
            );
            return Err(error);
        }
        if let Err(error) = ensure_empty_file(&stderr_path) {
            self.finish_failed_profile(
                profile_id,
                &request,
                ProfileCompletionReason::CollectorError,
                started_at,
                started.elapsed().as_millis(),
                None,
                &[],
                &[],
                error.to_string(),
            );
            return Err(error);
        }

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
        let mut collector_results = Vec::new();
        for config in &request.collectors {
            let collector_dir = match self
                .artifacts
                .profile_collector_dir(profile_id, config.artifact_name())
            {
                Ok(collector_dir) => collector_dir,
                Err(error) => {
                    let error_text = error.to_string();
                    self.record_event(
                        profile_id,
                        "profile_error",
                        None,
                        Some(error_text.clone()),
                        collector_fields(config),
                    );
                    collector_results.extend(stop_started_collectors_for_start_failure(
                        self,
                        profile_id,
                        &mut started_collectors,
                    ));
                    collector_results.push(failed_collector_result(config, error_text.clone()));
                    self.finish_failed_profile(
                        profile_id,
                        &request,
                        ProfileCompletionReason::CollectorError,
                        started_at,
                        started.elapsed().as_millis(),
                        None,
                        &collector_results,
                        &warnings,
                        error_text,
                    );
                    return Err(error);
                }
            };
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
                    self.record_event(
                        profile_id,
                        "profile_error",
                        Some(collector_dir),
                        Some(error.to_string()),
                        collector_fields(config),
                    );
                    collector_results.extend(stop_started_collectors_for_start_failure(
                        self,
                        profile_id,
                        &mut started_collectors,
                    ));
                    collector_results.push(failed_collector_result(config, error.to_string()));
                    self.finish_failed_profile(
                        profile_id,
                        &request,
                        ProfileCompletionReason::CollectorError,
                        started_at,
                        started.elapsed().as_millis(),
                        None,
                        &collector_results,
                        &warnings,
                        error.to_string(),
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
                    started_collectors.push(Arc::from(collector));
                }
                Err(error) => {
                    let mut fields = collector_fields(config);
                    match collector.cleanup() {
                        Ok(()) => self.record_event(
                            profile_id,
                            "collector_cleanup_finished",
                            Some(collector_dir.clone()),
                            None,
                            fields.clone(),
                        ),
                        Err(cleanup_error) => {
                            fields.insert(
                                "cleanup_error".to_string(),
                                Value::String(cleanup_error.to_string()),
                            );
                            self.record_event(
                                profile_id,
                                "collector_cleanup_failed",
                                Some(collector_dir.clone()),
                                Some(cleanup_error.to_string()),
                                fields.clone(),
                            );
                        }
                    }
                    collector_results.extend(stop_started_collectors_for_start_failure(
                        self,
                        profile_id,
                        &mut started_collectors,
                    ));
                    self.record_event(
                        profile_id,
                        "profile_error",
                        None,
                        Some(error.to_string()),
                        collector_fields(config),
                    );
                    collector_results.push(failed_collector_result(config, error.to_string()));
                    self.finish_failed_profile(
                        profile_id,
                        &request,
                        ProfileCompletionReason::CollectorError,
                        started_at,
                        started.elapsed().as_millis(),
                        None,
                        &collector_results,
                        &warnings,
                        error.to_string(),
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
            ProcessLaunchContext::new(self.process_launch.clone(), tool_context),
            self.logger.clone(),
            Arc::new(ProfileTargetEventSink {
                manager: self.clone(),
                profile_id,
                collectors: started_collectors.clone(),
            }),
        );

        let target_pid_for_collectors = target_pid_from_exit(&target_exit);
        let mut stop_error = None;
        for collector in started_collectors.into_iter().rev() {
            let name = collector.name().to_string();
            let kind = collector.kind();
            let mut fields = Map::new();
            fields.insert("collector".to_string(), Value::String(name.clone()));
            self.record_event(profile_id, "collector_stopping", None, None, fields.clone());
            match collector.stop(target_pid_for_collectors) {
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
                    let mut fields = fields;
                    match collector.cleanup() {
                        Ok(()) => {
                            self.record_event(
                                profile_id,
                                "collector_cleanup_finished",
                                None,
                                None,
                                fields.clone(),
                            );
                        }
                        Err(cleanup_error) => {
                            let cleanup_error = cleanup_error.to_string();
                            warnings.push(format!(
                                "collector {name} cleanup after stop failure failed: {cleanup_error}"
                            ));
                            fields.insert(
                                "cleanup_error".to_string(),
                                Value::String(cleanup_error.clone()),
                            );
                            self.record_event(
                                profile_id,
                                "collector_cleanup_failed",
                                None,
                                Some(cleanup_error),
                                fields.clone(),
                            );
                        }
                    }
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

        let metadata_artifact = match self.write_metadata(
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
        ) {
            Ok(artifact) => artifact,
            Err(metadata_error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "profile", "profile_metadata_write_failed")
                        .field("profile_id", profile_id.to_string())
                        .duration_ms(started.elapsed().as_millis())
                        .error(metadata_error.to_string()),
                );
                return Err(metadata_error);
            }
        };

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
        let mut completed_fields = Map::new();
        completed_fields.insert("status".to_string(), Value::String(format!("{status:?}")));
        completed_fields.insert(
            "completion_reason".to_string(),
            Value::String(format!("{completion_reason:?}")),
        );
        completed_fields.insert(
            "duration_ms".to_string(),
            Value::Number(serde_json::Number::from(duration_ms as u64)),
        );
        self.record_event(
            profile_id,
            "profile_completed",
            None,
            None,
            completed_fields,
        );
        self.log(
            LogEvent::new(LogLevel::Info, "profile", "run_profile_finished")
                .field("profile_id", profile_id.to_string())
                .duration_ms(duration_ms)
                .field("status", format!("{status:?}"))
                .field("completion_reason", format!("{completion_reason:?}"))
                .field("target_pid", target_pid)
                .field("target_exit_code", target_exit_code)
                .field("warnings_count", result.warnings.len())
                .field("error", result.error.clone())
                .field(
                    "metadata_path",
                    result.artifacts.profile.path.display().to_string(),
                ),
        );
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

    fn finish_failed_profile(
        &self,
        profile_id: ProfileId,
        request: &RunProfile,
        completion_reason: ProfileCompletionReason,
        started_at_unix_ms: u128,
        duration_ms: u128,
        trace_artifact: Option<&ArtifactRef>,
        collector_results: &[ProfileCollectorResult],
        warnings: &[String],
        error: String,
    ) {
        let mut fields = Map::new();
        fields.insert("status".to_string(), Value::String("Failed".to_string()));
        fields.insert(
            "completion_reason".to_string(),
            Value::String(format!("{completion_reason:?}")),
        );
        fields.insert(
            "duration_ms".to_string(),
            Value::Number(serde_json::Number::from(duration_ms as u64)),
        );
        let metadata_path = self.artifacts.profile_metadata_path(profile_id);
        let events_path = self.artifacts.profile_events_path(profile_id);
        self.record_event(
            profile_id,
            "profile_failed",
            None,
            Some(error.clone()),
            fields,
        );
        match self.write_metadata(
            profile_id,
            request,
            ProfileStatus::Failed,
            completion_reason,
            None,
            None,
            started_at_unix_ms,
            duration_ms,
            trace_artifact,
            collector_results,
            warnings,
            Some(error.clone()),
        ) {
            Ok(artifact) => self.record_event(
                profile_id,
                "profile_metadata_written",
                Some(artifact.path),
                None,
                Map::new(),
            ),
            Err(metadata_error) => self.log(
                LogEvent::new(LogLevel::Error, "profile", "profile_metadata_write_failed")
                    .field("profile_id", profile_id.to_string())
                    .duration_ms(duration_ms)
                    .error(metadata_error.to_string()),
            ),
        }
        self.log(
            LogEvent::new(LogLevel::Error, "profile", "run_profile_failed")
                .field("profile_id", profile_id.to_string())
                .duration_ms(duration_ms)
                .field("completion_reason", format!("{completion_reason:?}"))
                .field("warnings_count", warnings.len())
                .field("metadata_path", metadata_path.display().to_string())
                .field("events_path", events_path.display().to_string())
                .error(error),
        );
    }

    fn log_profile_rejected(&self, request: &RunProfile, duration_ms: u128, error: &DbgFlowError) {
        self.log(
            LogEvent::new(LogLevel::Error, "profile", "run_profile_rejected")
                .duration_ms(duration_ms)
                .field("target", &request.target)
                .field("timeout_ms", request.timeout_ms)
                .field("collectors", collector_names(request))
                .error(error.to_string()),
        );
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }

    fn record_target_started(&self, profile_id: ProfileId, pid: u32) {
        let mut fields = Map::new();
        fields.insert(
            "pid".to_string(),
            Value::Number(serde_json::Number::from(pid)),
        );
        self.record_event(profile_id, "target_started", None, None, fields);
        self.log(
            LogEvent::new(LogLevel::Info, "profile", "target_started")
                .field("profile_id", profile_id.to_string())
                .field("pid", pid),
        );
    }

    fn record_event(
        &self,
        profile_id: ProfileId,
        event: &str,
        artifact_path: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        if let Err(error) = self.artifacts.append_profile_event(
            profile_id,
            &ProfileArtifactEvent {
                timestamp_unix_ms: now_unix_ms(),
                event: event.to_string(),
                profile_id: profile_id.to_string(),
                artifact_path,
                error,
                fields,
            },
        ) {
            self.log(
                LogEvent::new(LogLevel::Warn, "profile", "profile_artifact_event_failed")
                    .field("profile_id", profile_id.to_string())
                    .field("event", event)
                    .error(error.to_string()),
            );
        }
    }
}

struct ProfileTargetEventSink {
    manager: ProfileManager,
    profile_id: ProfileId,
    collectors: Vec<Arc<dyn ProfileCollector>>,
}

impl TargetEventSink for ProfileTargetEventSink {
    fn target_started(&self, pid: u32) {
        self.manager.record_target_started(self.profile_id, pid);
        for collector in &self.collectors {
            collector.target_started(pid);
        }
    }
}

fn ensure_empty_file(path: &PathBuf) -> Result<()> {
    fs::write(path, b"").map_err(|error| DbgFlowError::Artifact(error.to_string()))
}

fn profile_request_fields(request: &RunProfile) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "target".to_string(),
        serde_json::to_value(&request.target)
            .unwrap_or_else(|_| Value::String("<serialize error>".to_string())),
    );
    fields.insert(
        "timeout_ms".to_string(),
        Value::Number(serde_json::Number::from(request.timeout_ms)),
    );
    fields.insert(
        "collectors".to_string(),
        serde_json::to_value(collector_names(request))
            .unwrap_or_else(|_| Value::String("<serialize error>".to_string())),
    );
    fields
}

fn collector_names(request: &RunProfile) -> Vec<&'static str> {
    request
        .collectors
        .iter()
        .map(|collector| collector.artifact_name())
        .collect()
}

fn collector_fields(config: &super::ProfileCollectorConfig) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "collector".to_string(),
        Value::String(config.artifact_name().to_string()),
    );
    fields
}

fn failed_collector_result(
    config: &super::ProfileCollectorConfig,
    error: String,
) -> ProfileCollectorResult {
    ProfileCollectorResult {
        kind: config.kind(),
        name: config.artifact_name().to_string(),
        status: ProfileCollectorStatus::Failed,
        artifacts: Vec::new(),
        warnings: Vec::new(),
        error: Some(error),
    }
}

fn reject_duplicate_collectors(request: &RunProfile) -> Result<()> {
    let mut seen = Vec::<ProfileCollectorKind>::new();
    for collector in &request.collectors {
        let kind = collector.kind();
        if seen.contains(&kind) {
            return Err(DbgFlowError::Backend(format!(
                "duplicate profile collector kind is not supported: {:?}",
                kind
            )));
        }
        seen.push(kind);
    }
    Ok(())
}

fn target_pid_from_exit(target_exit: &Result<TargetExit>) -> Option<u32> {
    match target_exit {
        Ok(TargetExit::Exited { pid, .. }) | Ok(TargetExit::TimedOut { pid }) => Some(*pid),
        Err(_) => None,
    }
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
    started_collectors: &mut Vec<Arc<dyn ProfileCollector>>,
) -> Vec<ProfileCollectorResult> {
    let mut results = Vec::new();
    while let Some(collector) = started_collectors.pop() {
        let name = collector.name().to_string();
        let kind = collector.kind();
        let mut fields = Map::new();
        fields.insert("collector".to_string(), Value::String(name.clone()));
        manager.record_event(profile_id, "collector_stopping", None, None, fields.clone());
        match collector.stop(None) {
            Ok(stop) => {
                manager.record_event(profile_id, "collector_stopped", None, None, fields);
                results.push(ProfileCollectorResult {
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
                manager.record_event(
                    profile_id,
                    "profile_error",
                    None,
                    Some(error.clone()),
                    fields,
                );
                results.push(ProfileCollectorResult {
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
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{CollectorStart, CollectorStop, ProfileCollectorConfig, ProfileTarget};
    use dbgflow_common::logging::noop_logger;
    use std::path::Path;
    use std::sync::Mutex;

    #[test]
    fn run_profile_with_context_passes_process_launch_context_to_target_runner() {
        let root =
            std::env::temp_dir().join(format!("dbgflow-profile-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let runner = Arc::new(RecordingTargetRunner::default());
        let process_launch = ProcessLaunchConfig::installed_service_default();
        let tool_context = ToolCallContext {
            peer_pid: Some(2001),
            peer_session_id: Some(33),
        };
        let manager = ProfileManager::with_components_and_logger(
            &root,
            Arc::new(TestCollectorFactory),
            runner.clone(),
            noop_logger(),
            process_launch.clone(),
        );

        manager
            .run_profile_with_context(
                RunProfile {
                    target: ProfileTarget::Launch {
                        executable: std::env::current_exe().expect("current exe"),
                        args: Vec::new(),
                    },
                    timeout_ms: 1000,
                    collectors: Vec::new(),
                },
                tool_context,
            )
            .expect("run profile");

        let recorded = runner.last_context();
        assert_eq!(recorded.config, process_launch);
        assert_eq!(recorded.tool_call, tool_context);
    }

    #[derive(Default)]
    struct RecordingTargetRunner {
        contexts: Mutex<Vec<ProcessLaunchContext>>,
    }

    impl RecordingTargetRunner {
        fn last_context(&self) -> ProcessLaunchContext {
            self.contexts
                .lock()
                .expect("contexts")
                .last()
                .expect("context")
                .clone()
        }
    }

    impl TargetRunner for RecordingTargetRunner {
        fn launch_and_wait(
            &self,
            _target: &ProfileTarget,
            _timeout: Duration,
            _stdout_path: &Path,
            _stderr_path: &Path,
            launch_context: ProcessLaunchContext,
            _logger: Arc<dyn LogSink>,
            event_sink: Arc<dyn TargetEventSink>,
        ) -> Result<TargetExit> {
            self.contexts.lock().expect("contexts").push(launch_context);
            event_sink.target_started(99);
            Ok(TargetExit::Exited {
                pid: 99,
                exit_code: Some(0),
            })
        }
    }

    struct TestCollectorFactory;

    impl CollectorFactory for TestCollectorFactory {
        fn create(
            &self,
            _config: &ProfileCollectorConfig,
            _output_dir: &Path,
        ) -> Result<Box<dyn ProfileCollector>> {
            Ok(Box::new(TestCollector))
        }
    }

    struct TestCollector;

    impl ProfileCollector for TestCollector {
        fn name(&self) -> &str {
            "native_etw"
        }

        fn kind(&self) -> ProfileCollectorKind {
            ProfileCollectorKind::NativeEtw
        }

        fn start(&self) -> Result<CollectorStart> {
            Ok(CollectorStart {
                warnings: Vec::new(),
            })
        }

        fn stop(&self, _target_pid: Option<u32>) -> Result<CollectorStop> {
            Ok(CollectorStop {
                artifacts: Vec::new(),
                warnings: Vec::new(),
            })
        }

        fn cleanup(&self) -> Result<()> {
            Ok(())
        }
    }
}
