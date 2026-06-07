use super::ProfileTarget;
use crate::{DbgFlowError, Result};
use std::fs::File;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
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
    ) -> Result<TargetExit>;
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
    ) -> Result<TargetExit> {
        let ProfileTarget::Launch { executable, args } = target;
        let stdout = File::create(stdout_path).map_err(|error| {
            DbgFlowError::Artifact(format!("create target stdout failed: {error}"))
        })?;
        let stderr = File::create(stderr_path).map_err(|error| {
            DbgFlowError::Artifact(format!("create target stderr failed: {error}"))
        })?;
        let mut child = Command::new(executable)
            .args(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| {
                DbgFlowError::Backend(format!("launch profile target failed: {error}"))
            })?;
        let pid = child.id();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().map_err(|error| {
                DbgFlowError::Backend(format!("poll profile target failed: {error}"))
            })? {
                return Ok(TargetExit::Exited {
                    pid,
                    exit_code: exit_code(status),
                });
            }
            if Instant::now() >= deadline {
                return Ok(TargetExit::TimedOut { pid });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}
