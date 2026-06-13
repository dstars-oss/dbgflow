use crate::backend::DebugTarget;
use dbgflow_common::{DbgFlowError, Result};
use std::path::{Path, PathBuf};

pub(super) fn validate_target(target: DebugTarget) -> Result<DebugTarget> {
    match target {
        DebugTarget::Dump { path } => {
            validate_dump_target(&path).map(|path| DebugTarget::Dump { path })
        }
        DebugTarget::Attach { pid } => validate_attach_target(pid),
        DebugTarget::Launch { executable, args } => validate_launch_target(&executable, args),
    }
}

fn validate_dump_target(path: &Path) -> Result<PathBuf> {
    let canonical_path = path
        .canonicalize()
        .map_err(|error| DbgFlowError::Backend(format!("invalid dump path: {error}")))?;
    if !canonical_path.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "dump path is not a file: {}",
            canonical_path.display()
        )));
    }

    Ok(canonical_path)
}

fn validate_attach_target(pid: u32) -> Result<DebugTarget> {
    if pid == 0 {
        return Err(DbgFlowError::Backend(
            "attach pid must be greater than zero".to_string(),
        ));
    }
    if pid == std::process::id() {
        return Err(DbgFlowError::Backend(
            "refusing to attach to the current dbgflow process".to_string(),
        ));
    }
    Ok(DebugTarget::Attach { pid })
}

fn validate_launch_target(executable: &Path, args: Vec<String>) -> Result<DebugTarget> {
    let executable = executable
        .canonicalize()
        .map_err(|error| DbgFlowError::Backend(format!("invalid launch executable: {error}")))?;
    if !executable.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "launch executable is not a file: {}",
            executable.display()
        )));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(DbgFlowError::Backend(
            "launch arguments must not contain NUL bytes".to_string(),
        ));
    }

    Ok(DebugTarget::Launch { executable, args })
}
