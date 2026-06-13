mod args;
mod validation;

pub use args::build_ttd_args;
use args::ttd_stop_target;
pub use validation::{validate_ttd_options, validate_ttd_target};

use dbgflow_common::artifacts::{
    ArtifactKind, ArtifactManager, ArtifactRef, TtdRecordingArtifactEvent,
};
use dbgflow_common::job::SingleActiveJob;
use dbgflow_common::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use dbgflow_common::time::now_unix_ms;
pub use dbgflow_common::TtdRecordingId;
use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordTtd {
    pub target: TtdTarget,
    pub timeout_ms: u64,
    #[serde(default)]
    pub options: TtdRecordingOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TtdTarget {
    Launch {
        executable: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    Attach {
        pid: u32,
    },
    Monitor {
        program: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cmd_line_filter: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtdRecordingOptions {
    pub children: bool,
    pub no_ui: bool,
    pub accept_eula: bool,
    pub ring: bool,
    pub max_file_mb: u32,
    pub modules: Vec<String>,
    pub record_mode: TtdRecordMode,
    pub replay_cpu_support: TtdReplayCpuSupport,
}

impl Default for TtdRecordingOptions {
    fn default() -> Self {
        Self {
            children: false,
            no_ui: true,
            accept_eula: false,
            ring: false,
            max_file_mb: 2048,
            modules: Vec::new(),
            record_mode: TtdRecordMode::Automatic,
            replay_cpu_support: TtdReplayCpuSupport::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtdRecordMode {
    Automatic,
    Manual,
}

impl TtdRecordMode {
    fn as_ttd_arg(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::Manual => "Manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtdReplayCpuSupport {
    Default,
    MostConservative,
    MostAggressive,
    IntelAvxRequired,
    IntelAvx2Required,
}

impl TtdReplayCpuSupport {
    fn as_ttd_arg(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::MostConservative => "MostConservative",
            Self::MostAggressive => "MostAggressive",
            Self::IntelAvxRequired => "IntelAvxRequired",
            Self::IntelAvx2Required => "IntelAvx2Required",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtdRecordingStatus {
    Completed,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtdRecordingCompletionReason {
    TargetExited,
    Timeout,
    RecorderError,
    RecorderUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtdRecordingArtifacts {
    pub metadata: ArtifactRef,
    pub events: ArtifactRef,
    pub traces: Vec<ArtifactRef>,
    pub trace_indexes: Vec<ArtifactRef>,
    pub recorder_stdout: ArtifactRef,
    pub recorder_stderr: ArtifactRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder_stop_stdout: Option<ArtifactRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder_stop_stderr: Option<ArtifactRef>,
    pub recorder_logs: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtdRecordingResult {
    pub recording_id: TtdRecordingId,
    pub status: TtdRecordingStatus,
    pub completion_reason: TtdRecordingCompletionReason,
    pub duration_ms: u128,
    pub target_pid: Option<u32>,
    pub recorder_exit_code: Option<i32>,
    pub artifacts: TtdRecordingArtifacts,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TtdRecorderRuntime {
    ttd_dir: Option<PathBuf>,
}

impl TtdRecorderRuntime {
    pub fn unavailable() -> Self {
        Self { ttd_dir: None }
    }

    pub fn with_ttd_dir(path: PathBuf) -> Self {
        Self {
            ttd_dir: Some(path),
        }
    }

    pub fn ttd_dir(&self) -> Option<&Path> {
        self.ttd_dir.as_deref()
    }

    pub fn ttd_exe(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.ttd_dir {
            let exe = dir.join("TTD.exe");
            if exe.is_file() {
                return Ok(exe);
            }
            return Err(DbgFlowError::Backend(format!(
                "TTD recorder requires TTD.exe under {}",
                dir.display()
            )));
        }

        find_ttd_exe_in_path()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtdRecorderInvocation {
    pub ttd_exe: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtdStopInvocation {
    pub ttd_exe: PathBuf,
    pub stop_target: OsString,
    pub timeout: Duration,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtdRecorderExit {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

pub trait TtdRecorderRunner: Send + Sync {
    fn run(&self, invocation: TtdRecorderInvocation) -> Result<TtdRecorderExit>;
    fn stop(&self, invocation: TtdStopInvocation) -> Result<TtdRecorderExit>;
}

#[derive(Debug, Default)]
pub struct ProcessTtdRecorderRunner;

impl TtdRecorderRunner for ProcessTtdRecorderRunner {
    fn run(&self, invocation: TtdRecorderInvocation) -> Result<TtdRecorderExit> {
        run_ttd_process(
            &invocation.ttd_exe,
            &invocation.args,
            invocation.timeout,
            &invocation.stdout_path,
            &invocation.stderr_path,
        )
    }

    fn stop(&self, invocation: TtdStopInvocation) -> Result<TtdRecorderExit> {
        let args = [
            OsString::from("-stop"),
            invocation.stop_target,
            OsString::from("-wait"),
            OsString::from("10"),
        ];
        run_ttd_process(
            &invocation.ttd_exe,
            &args,
            invocation.timeout,
            &invocation.stdout_path,
            &invocation.stderr_path,
        )
    }
}

#[derive(Clone)]
pub struct TtdRecordingManager {
    artifacts: ArtifactManager,
    runtime: TtdRecorderRuntime,
    runner: Arc<dyn TtdRecorderRunner>,
    active_job: SingleActiveJob<TtdRecordingId>,
    logger: Arc<dyn LogSink>,
}

impl TtdRecordingManager {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self::with_runtime(artifact_root, TtdRecorderRuntime::unavailable())
    }

    pub fn with_runtime(artifact_root: impl Into<PathBuf>, runtime: TtdRecorderRuntime) -> Self {
        Self::with_runtime_and_logger(artifact_root, runtime, noop_logger())
    }

    pub fn with_runtime_and_logger(
        artifact_root: impl Into<PathBuf>,
        runtime: TtdRecorderRuntime,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self::with_components_and_logger(
            artifact_root,
            runtime,
            Arc::new(ProcessTtdRecorderRunner),
            logger,
        )
    }

    pub fn with_components(
        artifact_root: impl Into<PathBuf>,
        runtime: TtdRecorderRuntime,
        runner: Arc<dyn TtdRecorderRunner>,
    ) -> Self {
        Self::with_components_and_logger(artifact_root, runtime, runner, noop_logger())
    }

    pub fn with_components_and_logger(
        artifact_root: impl Into<PathBuf>,
        runtime: TtdRecorderRuntime,
        runner: Arc<dyn TtdRecorderRunner>,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self {
            artifacts: ArtifactManager::new(artifact_root),
            runtime,
            runner,
            active_job: SingleActiveJob::default(),
            logger,
        }
    }

    pub fn record_ttd(&self, mut request: RecordTtd) -> Result<TtdRecordingResult> {
        let request_started = Instant::now();
        let requested_target = request.target.clone();
        request.target = match validate_ttd_target(request.target) {
            Ok(target) => target,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "ttd", "record_ttd_rejected")
                        .duration_ms(request_started.elapsed().as_millis())
                        .field("target", requested_target)
                        .error(error.to_string()),
                );
                return Err(error);
            }
        };
        if request.timeout_ms == 0 {
            let error =
                DbgFlowError::Backend("TTD recording timeout_ms must be greater than zero".into());
            self.log_recording_rejected(&request, request_started.elapsed().as_millis(), &error);
            return Err(error);
        }
        if let Err(error) = validate_ttd_options(&request.options, &request.target) {
            self.log_recording_rejected(&request, request_started.elapsed().as_millis(), &error);
            return Err(error);
        }

        let ttd_exe = match self.runtime.ttd_exe() {
            Ok(ttd_exe) => ttd_exe,
            Err(error) => {
                self.log_recording_rejected(
                    &request,
                    request_started.elapsed().as_millis(),
                    &error,
                );
                return Err(error);
            }
        };

        let recording_id = TtdRecordingId::new();
        let active_guard = match self.active_job.start(recording_id, |active_id| {
            format!("another TTD recording job is already active: {active_id}")
        }) {
            Ok(guard) => guard,
            Err(error) => {
                self.log_recording_rejected(
                    &request,
                    request_started.elapsed().as_millis(),
                    &error,
                );
                return Err(error);
            }
        };

        let started = Instant::now();
        let started_at = now_unix_ms();
        let recording_dir = self
            .artifacts
            .initialize_ttd_recording_artifacts(recording_id)?;
        let traces_dir = self.artifacts.ttd_recording_traces_dir(recording_id);
        let recorder_stdout_path = self.artifacts.ttd_recorder_stdout_path(recording_id);
        let recorder_stderr_path = self.artifacts.ttd_recorder_stderr_path(recording_id);
        let recorder_stop_stdout_path = self.artifacts.ttd_recorder_stop_stdout_path(recording_id);
        let recorder_stop_stderr_path = self.artifacts.ttd_recorder_stop_stderr_path(recording_id);
        let events_artifact = ArtifactRef {
            kind: ArtifactKind::TtdRecordingEvents,
            path: self.artifacts.ttd_recording_events_path(recording_id),
        };
        let recorder_stdout_artifact = ArtifactRef {
            kind: ArtifactKind::TtdRecorderOutput,
            path: recorder_stdout_path.clone(),
        };
        let recorder_stderr_artifact = ArtifactRef {
            kind: ArtifactKind::TtdRecorderOutput,
            path: recorder_stderr_path.clone(),
        };

        self.record_event(
            recording_id,
            "ttd_recording_started",
            Some(recording_dir.clone()),
            None,
            recording_request_fields(&request),
        );
        self.log(
            LogEvent::new(LogLevel::Info, "ttd", "record_ttd_started")
                .field("recording_id", recording_id.to_string())
                .field("target", &request.target)
                .field("timeout_ms", request.timeout_ms)
                .field("ttd_exe", ttd_exe.display().to_string()),
        );

        let args = build_ttd_args(&request.target, &request.options, &traces_dir);
        self.record_event(
            recording_id,
            "recorder_starting",
            None,
            None,
            recorder_command_fields(&ttd_exe, &args),
        );
        let recorder_exit = self.runner.run(TtdRecorderInvocation {
            ttd_exe: ttd_exe.clone(),
            args,
            timeout: Duration::from_millis(request.timeout_ms),
            stdout_path: recorder_stdout_path.clone(),
            stderr_path: recorder_stderr_path.clone(),
        });

        let mut warnings = Vec::new();
        let mut error = None;
        let (recorder_exit_code, timed_out) = match recorder_exit {
            Ok(exit) => {
                let recorder_exit_code = exit.exit_code;
                let timed_out = exit.timed_out;
                self.record_event(
                    recording_id,
                    "recorder_finished",
                    None,
                    None,
                    recorder_exit_fields(&exit),
                );
                (recorder_exit_code, timed_out)
            }
            Err(run_error) => {
                let error_text = run_error.to_string();
                self.record_event(
                    recording_id,
                    "recording_failed",
                    None,
                    Some(error_text.clone()),
                    Map::new(),
                );
                let traces = discover_ttd_artifacts(&traces_dir)?;
                let duration_ms = started.elapsed().as_millis();
                let metadata_artifact = self.write_metadata(
                    recording_id,
                    &request,
                    TtdRecordingStatus::Failed,
                    TtdRecordingCompletionReason::RecorderError,
                    None,
                    None,
                    started_at,
                    duration_ms,
                    &traces,
                    &warnings,
                    Some(error_text.clone()),
                )?;
                self.log(
                    LogEvent::new(LogLevel::Error, "ttd", "record_ttd_failed")
                        .field("recording_id", recording_id.to_string())
                        .duration_ms(duration_ms)
                        .error(error_text.clone()),
                );
                drop(active_guard);
                return Ok(TtdRecordingResult {
                    recording_id,
                    status: TtdRecordingStatus::Failed,
                    completion_reason: TtdRecordingCompletionReason::RecorderError,
                    duration_ms,
                    target_pid: None,
                    recorder_exit_code: None,
                    artifacts: TtdRecordingArtifacts {
                        metadata: metadata_artifact,
                        events: events_artifact,
                        traces: traces.traces,
                        trace_indexes: traces.trace_indexes,
                        recorder_stdout: recorder_stdout_artifact,
                        recorder_stderr: recorder_stderr_artifact,
                        recorder_stop_stdout: None,
                        recorder_stop_stderr: None,
                        recorder_logs: traces.recorder_logs,
                    },
                    warnings,
                    error: Some(error_text),
                });
            }
        };

        let output_text = read_text_lossy(&recorder_stdout_path)
            + "\n"
            + read_text_lossy(&recorder_stderr_path).as_str();
        let target_pid = parse_first_recorded_pid(&output_text);
        let mut recorder_stop_stdout_artifact = None;
        let mut recorder_stop_stderr_artifact = None;

        if timed_out {
            self.record_event(recording_id, "timeout_reached", None, None, Map::new());
            match ttd_stop_target(&request.target, target_pid) {
                Some(stop_target) => {
                    recorder_stop_stdout_artifact = Some(ArtifactRef {
                        kind: ArtifactKind::TtdRecorderOutput,
                        path: recorder_stop_stdout_path.clone(),
                    });
                    recorder_stop_stderr_artifact = Some(ArtifactRef {
                        kind: ArtifactKind::TtdRecorderOutput,
                        path: recorder_stop_stderr_path.clone(),
                    });
                    match self.runner.stop(TtdStopInvocation {
                        ttd_exe: ttd_exe.clone(),
                        stop_target: stop_target.clone(),
                        timeout: Duration::from_secs(15),
                        stdout_path: recorder_stop_stdout_path.clone(),
                        stderr_path: recorder_stop_stderr_path.clone(),
                    }) {
                        Ok(stop) => {
                            let mut fields = recorder_exit_fields(&stop);
                            fields.insert(
                                "stop_target".to_string(),
                                Value::String(stop_target.to_string_lossy().into_owned()),
                            );
                            self.record_event(
                                recording_id,
                                "recorder_stop_finished",
                                None,
                                None,
                                fields,
                            );
                        }
                        Err(stop_error) => {
                            let warning =
                                format!("TTD stop failed after recorder timeout: {stop_error}");
                            warnings.push(warning.clone());
                            let mut fields = Map::new();
                            fields.insert(
                                "stop_target".to_string(),
                                Value::String(stop_target.to_string_lossy().into_owned()),
                            );
                            self.record_event(
                                recording_id,
                                "recorder_stop_failed",
                                None,
                                Some(stop_error.to_string()),
                                fields,
                            );
                        }
                    }
                }
                None => {
                    warnings.push(
                        "TTD recorder timed out, but dbgflow could not determine a safe stop target"
                            .to_string(),
                    );
                    self.record_event(
                        recording_id,
                        "recorder_stop_skipped",
                        None,
                        Some("could not determine a safe stop target".to_string()),
                        Map::new(),
                    );
                }
            }
        }

        let discovered = discover_ttd_artifacts(&traces_dir)?;
        for artifact in discovered
            .traces
            .iter()
            .chain(discovered.trace_indexes.iter())
            .chain(discovered.recorder_logs.iter())
        {
            self.record_event(
                recording_id,
                "trace_detected",
                Some(artifact.path.clone()),
                None,
                artifact_fields(artifact),
            );
        }

        let duration_ms = started.elapsed().as_millis();

        let (status, completion_reason) = if timed_out {
            (
                TtdRecordingStatus::TimedOut,
                TtdRecordingCompletionReason::Timeout,
            )
        } else if recorder_exit_code.is_some_and(|code| code != 0) {
            error = Some(recorder_error_summary(
                &recorder_stdout_path,
                &recorder_stderr_path,
                recorder_exit_code,
            ));
            (
                TtdRecordingStatus::Failed,
                TtdRecordingCompletionReason::RecorderError,
            )
        } else if discovered.traces.is_empty() {
            error = Some("TTD recorder completed but no .run trace was created".to_string());
            (
                TtdRecordingStatus::Failed,
                TtdRecordingCompletionReason::RecorderError,
            )
        } else {
            (
                TtdRecordingStatus::Completed,
                TtdRecordingCompletionReason::TargetExited,
            )
        };

        let metadata_artifact = self.write_metadata(
            recording_id,
            &request,
            status,
            completion_reason,
            target_pid,
            recorder_exit_code,
            started_at,
            duration_ms,
            &discovered,
            &warnings,
            error.clone(),
        )?;

        let result = TtdRecordingResult {
            recording_id,
            status,
            completion_reason,
            duration_ms,
            target_pid,
            recorder_exit_code,
            artifacts: TtdRecordingArtifacts {
                metadata: metadata_artifact,
                events: events_artifact,
                traces: discovered.traces,
                trace_indexes: discovered.trace_indexes,
                recorder_stdout: recorder_stdout_artifact,
                recorder_stderr: recorder_stderr_artifact,
                recorder_stop_stdout: recorder_stop_stdout_artifact,
                recorder_stop_stderr: recorder_stop_stderr_artifact,
                recorder_logs: discovered.recorder_logs,
            },
            warnings,
            error,
        };

        self.record_event(
            recording_id,
            "recording_completed",
            None,
            result.error.clone(),
            recording_completed_fields(&result),
        );
        let log_level = if result.status == TtdRecordingStatus::Failed {
            LogLevel::Error
        } else {
            LogLevel::Info
        };
        self.log(
            LogEvent::new(log_level, "ttd", "record_ttd_finished")
                .field("recording_id", recording_id.to_string())
                .duration_ms(duration_ms)
                .field("status", format!("{:?}", result.status))
                .field(
                    "completion_reason",
                    format!("{:?}", result.completion_reason),
                )
                .field("target_pid", result.target_pid)
                .field("recorder_exit_code", result.recorder_exit_code)
                .field("trace_count", result.artifacts.traces.len())
                .field("warnings_count", result.warnings.len())
                .field("error", result.error.clone())
                .field(
                    "metadata_path",
                    result.artifacts.metadata.path.display().to_string(),
                ),
        );
        drop(active_guard);
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_metadata(
        &self,
        recording_id: TtdRecordingId,
        request: &RecordTtd,
        status: TtdRecordingStatus,
        completion_reason: TtdRecordingCompletionReason,
        target_pid: Option<u32>,
        recorder_exit_code: Option<i32>,
        started_at_unix_ms: u128,
        duration_ms: u128,
        discovered: &DiscoveredTtdArtifacts,
        warnings: &[String],
        error: Option<String>,
    ) -> Result<ArtifactRef> {
        let metadata = json!({
            "recording_id": recording_id.to_string(),
            "target": request.target,
            "timeout_ms": request.timeout_ms,
            "options": request.options,
            "target_pid": target_pid,
            "start_time_unix_ms": started_at_unix_ms,
            "end_time_unix_ms": now_unix_ms(),
            "duration_ms": duration_ms,
            "status": status,
            "completion_reason": completion_reason,
            "recorder_exit_code": recorder_exit_code,
            "traces": discovered.traces.iter().map(|artifact| artifact.path.clone()).collect::<Vec<_>>(),
            "trace_indexes": discovered.trace_indexes.iter().map(|artifact| artifact.path.clone()).collect::<Vec<_>>(),
            "recorder_logs": discovered.recorder_logs.iter().map(|artifact| artifact.path.clone()).collect::<Vec<_>>(),
            "warnings": warnings,
            "error": error,
        });
        self.artifacts
            .write_ttd_recording_metadata(recording_id, &metadata)
    }

    fn log_recording_rejected(&self, request: &RecordTtd, duration_ms: u128, error: &DbgFlowError) {
        self.log(
            LogEvent::new(LogLevel::Error, "ttd", "record_ttd_rejected")
                .duration_ms(duration_ms)
                .field("target", &request.target)
                .field("timeout_ms", request.timeout_ms)
                .field("options", &request.options)
                .error(error.to_string()),
        );
    }

    fn record_event(
        &self,
        recording_id: TtdRecordingId,
        event: &str,
        artifact_path: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        if let Err(error) = self.artifacts.append_ttd_recording_event(
            recording_id,
            &TtdRecordingArtifactEvent {
                timestamp_unix_ms: now_unix_ms(),
                event: event.to_string(),
                recording_id: recording_id.to_string(),
                artifact_path,
                error,
                fields,
            },
        ) {
            self.log(
                LogEvent::new(LogLevel::Warn, "ttd", "ttd_artifact_event_failed")
                    .field("recording_id", recording_id.to_string())
                    .field("event", event)
                    .error(error.to_string()),
            );
        }
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }
}

fn run_ttd_process(
    ttd_exe: &Path,
    args: &[OsString],
    timeout: Duration,
    stdout_path: &Path,
    stderr_path: &Path,
) -> Result<TtdRecorderExit> {
    let stdout = File::create(stdout_path)
        .map_err(|error| DbgFlowError::Artifact(format!("create TTD stdout failed: {error}")))?;
    let stderr = File::create(stderr_path)
        .map_err(|error| DbgFlowError::Artifact(format!("create TTD stderr failed: {error}")))?;
    let mut child = Command::new(ttd_exe)
        .args(args)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| DbgFlowError::Backend(format!("start TTD recorder failed: {error}")))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| DbgFlowError::Backend(format!("poll TTD recorder failed: {error}")))?
        {
            return Ok(TtdRecorderExit {
                exit_code: status.code(),
                timed_out: false,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().ok();
            return Ok(TtdRecorderExit {
                exit_code: status.and_then(|status| status.code()),
                timed_out: true,
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
fn find_ttd_exe_in_path() -> Result<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("TTD.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(DbgFlowError::Backend(
        "TTD.exe was not found in PATH; configure tools.ttd_dir or debugger.dbgeng_dir".to_string(),
    ))
}

#[cfg(not(windows))]
fn find_ttd_exe_in_path() -> Result<PathBuf> {
    Err(DbgFlowError::Backend(
        "TTD recording is only supported on Windows".to_string(),
    ))
}

#[derive(Debug, Default)]
struct DiscoveredTtdArtifacts {
    traces: Vec<ArtifactRef>,
    trace_indexes: Vec<ArtifactRef>,
    recorder_logs: Vec<ArtifactRef>,
}

fn discover_ttd_artifacts(traces_dir: &Path) -> Result<DiscoveredTtdArtifacts> {
    let mut discovered = DiscoveredTtdArtifacts::default();
    if !traces_dir.exists() {
        return Ok(discovered);
    }
    for entry in
        fs::read_dir(traces_dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?
    {
        let entry = entry.map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "run" => discovered.traces.push(ArtifactRef {
                kind: ArtifactKind::TtdTrace,
                path,
            }),
            "idx" => discovered.trace_indexes.push(ArtifactRef {
                kind: ArtifactKind::TtdTraceIndex,
                path,
            }),
            "out" | "err" => discovered.recorder_logs.push(ArtifactRef {
                kind: ArtifactKind::TtdRecorderOutput,
                path,
            }),
            _ => {}
        }
    }
    discovered
        .traces
        .sort_by(|left, right| left.path.cmp(&right.path));
    discovered
        .trace_indexes
        .sort_by(|left, right| left.path.cmp(&right.path));
    discovered
        .recorder_logs
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(discovered)
}

fn parse_first_recorded_pid(text: &str) -> Option<u32> {
    for line in text.lines() {
        if let Some(index) = line.find("PID:") {
            let digits = line[index + 4..]
                .chars()
                .skip_while(|ch| ch.is_whitespace())
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if let Ok(pid) = digits.parse::<u32>() {
                return Some(pid);
            }
        }
        if line.contains("Recording process ") {
            if let Some(open) = line.rfind('(') {
                if let Some(close) = line[open + 1..].find(')') {
                    let value = &line[open + 1..open + 1 + close];
                    if let Ok(pid) = value.parse::<u32>() {
                        return Some(pid);
                    }
                }
            }
        }
    }
    None
}

fn recorder_error_summary(
    stdout_path: &Path,
    stderr_path: &Path,
    exit_code: Option<i32>,
) -> String {
    let stderr = read_text_lossy(stderr_path);
    if !stderr.trim().is_empty() {
        return format!(
            "TTD recorder exited with code {:?}: {}",
            exit_code,
            last_non_empty_line(&stderr)
        );
    }
    let stdout = read_text_lossy(stdout_path);
    if !stdout.trim().is_empty() {
        return format!(
            "TTD recorder exited with code {:?}: {}",
            exit_code,
            last_non_empty_line(&stdout)
        );
    }
    format!("TTD recorder exited with code {:?}", exit_code)
}

fn read_text_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn last_non_empty_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn recording_request_fields(request: &RecordTtd) -> Map<String, Value> {
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
        "options".to_string(),
        serde_json::to_value(&request.options)
            .unwrap_or_else(|_| Value::String("<serialize error>".to_string())),
    );
    fields
}

fn recorder_command_fields(ttd_exe: &Path, args: &[OsString]) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "ttd_exe".to_string(),
        Value::String(ttd_exe.display().to_string()),
    );
    fields.insert(
        "args".to_string(),
        Value::Array(
            args.iter()
                .map(|arg| Value::String(arg.to_string_lossy().into_owned()))
                .collect(),
        ),
    );
    fields
}

fn recorder_exit_fields(exit: &TtdRecorderExit) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("timed_out".to_string(), Value::Bool(exit.timed_out));
    fields.insert(
        "exit_code".to_string(),
        exit.exit_code
            .map(|code| Value::Number(serde_json::Number::from(code)))
            .unwrap_or(Value::Null),
    );
    fields
}

fn artifact_fields(artifact: &ArtifactRef) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "kind".to_string(),
        Value::String(format!("{:?}", artifact.kind)),
    );
    fields
}

fn recording_completed_fields(result: &TtdRecordingResult) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "status".to_string(),
        Value::String(format!("{:?}", result.status)),
    );
    fields.insert(
        "completion_reason".to_string(),
        Value::String(format!("{:?}", result.completion_reason)),
    );
    fields.insert(
        "duration_ms".to_string(),
        Value::Number(serde_json::Number::from(result.duration_ms as u64)),
    );
    fields.insert(
        "trace_count".to_string(),
        Value::Number(serde_json::Number::from(
            result.artifacts.traces.len() as u64
        )),
    );
    fields
}
