use super::ProfileTarget;
use crate::{DbgFlowError, Result};
use std::path::Path;

pub fn validate_profile_target(target: ProfileTarget) -> Result<ProfileTarget> {
    match target {
        ProfileTarget::Launch { executable, args } => validate_launch_target(&executable, args),
    }
}

fn validate_launch_target(executable: &Path, args: Vec<String>) -> Result<ProfileTarget> {
    let executable = executable
        .canonicalize()
        .map_err(|error| DbgFlowError::Backend(format!("invalid profile launch executable: {error}")))?;
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
