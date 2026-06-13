use super::ProfileTarget;
use dbgflow_common::logging::LogSink;
use dbgflow_common::process::{
    log_process_launch, spawn_process, LaunchStdio, ProcessLaunchContext, ProcessLaunchSpec,
};
use dbgflow_common::{DbgFlowError, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn validate_profile_target(target: ProfileTarget) -> Result<ProfileTarget> {
    match target {
        ProfileTarget::Launch { executable, args } => validate_launch_target(&executable, args),
    }
}

fn validate_launch_target(executable: &Path, args: Vec<String>) -> Result<ProfileTarget> {
    let executable = executable.canonicalize().map_err(|error| {
        DbgFlowError::Backend(format!("invalid profile launch executable: {error}"))
    })?;
    if !executable.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "profile launch executable is not a file: {}",
            executable.display()
        )));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(DbgFlowError::Backend(
            "profile launch arguments must not contain NUL bytes".to_string(),
        ));
    }
    Ok(ProfileTarget::Launch { executable, args })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetExit {
    Exited { pid: u32, exit_code: Option<i32> },
    TimedOut { pid: u32 },
}

pub trait TargetRunner: Send + Sync {
    fn launch_and_wait(
        &self,
        target: &ProfileTarget,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
        launch_context: ProcessLaunchContext,
        logger: Arc<dyn LogSink>,
        event_sink: Arc<dyn TargetEventSink>,
    ) -> Result<TargetExit>;
}

pub trait TargetEventSink: Send + Sync {
    fn target_started(&self, pid: u32);
}

#[derive(Debug, Default)]
pub struct NoopTargetEventSink;

impl TargetEventSink for NoopTargetEventSink {
    fn target_started(&self, _pid: u32) {}
}

#[derive(Debug, Default)]
pub struct ProcessTargetRunner;

impl TargetRunner for ProcessTargetRunner {
    fn launch_and_wait(
        &self,
        target: &ProfileTarget,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
        launch_context: ProcessLaunchContext,
        logger: Arc<dyn LogSink>,
        event_sink: Arc<dyn TargetEventSink>,
    ) -> Result<TargetExit> {
        let ProfileTarget::Launch { executable, args } = target;
        let mut spec = ProcessLaunchSpec::new(executable);
        spec.args = args.iter().map(Into::into).collect();
        spec.stdout = LaunchStdio::File(stdout_path.to_path_buf());
        spec.stderr = LaunchStdio::File(stderr_path.to_path_buf());
        let mut child = spawn_process(&spec, &launch_context).map_err(|error| {
            DbgFlowError::Backend(format!("launch profile target failed: {error}"))
        })?;
        let pid = child.pid();
        log_process_launch(
            &logger,
            "profile",
            "target_process_launch_resolved",
            child.audit(),
        );
        event_sink.target_started(pid);
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(TargetExit::Exited {
                    pid,
                    exit_code: status.code,
                });
            }
            if Instant::now() >= deadline {
                return Ok(TargetExit::TimedOut { pid });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
