use super::{TtdRecordingOptions, TtdTarget};
use dbgflow_common::validation::{
    has_parent_component, is_absolute_path_text, path_text_has_separator,
    split_path_text_has_parent, validate_plain_text,
};
use dbgflow_common::{DbgFlowError, Result};
use std::path::{Path, PathBuf};

pub fn validate_ttd_target(target: TtdTarget) -> Result<TtdTarget> {
    match target {
        TtdTarget::Launch { executable, args } => validate_launch_target(&executable, args),
        TtdTarget::Attach { pid } => validate_attach_target(pid),
        TtdTarget::Monitor {
            program,
            cmd_line_filter,
        } => validate_monitor_target(program, cmd_line_filter),
    }
}

pub fn validate_ttd_options(options: &TtdRecordingOptions, target: &TtdTarget) -> Result<()> {
    if options.max_file_mb == 0 {
        return Err(DbgFlowError::Backend(
            "TTD max_file_mb must be greater than zero".to_string(),
        ));
    }
    if options.ring && options.max_file_mb > 32768 {
        return Err(DbgFlowError::Backend(
            "TTD ring max_file_mb must be at most 32768".to_string(),
        ));
    }
    if !options.ring && options.max_file_mb > 1_048_576 {
        return Err(DbgFlowError::Backend(
            "TTD max_file_mb must be at most 1048576".to_string(),
        ));
    }
    for module in &options.modules {
        validate_plain_text(module, "TTD module")?;
        if module.trim().is_empty() {
            return Err(DbgFlowError::Backend(
                "TTD module must not be empty".to_string(),
            ));
        }
    }
    if !matches!(target, TtdTarget::Monitor { .. }) && options.modules.len() > 64 {
        return Err(DbgFlowError::Backend(
            "TTD modules list is too large".to_string(),
        ));
    }
    Ok(())
}

fn validate_launch_target(executable: &Path, args: Vec<String>) -> Result<TtdTarget> {
    let executable = executable.canonicalize().map_err(|error| {
        DbgFlowError::Backend(format!("invalid TTD launch executable: {error}"))
    })?;
    if !executable.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "TTD launch executable is not a file: {}",
            executable.display()
        )));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(DbgFlowError::Backend(
            "TTD launch arguments must not contain NUL bytes".to_string(),
        ));
    }
    Ok(TtdTarget::Launch { executable, args })
}

fn validate_attach_target(pid: u32) -> Result<TtdTarget> {
    if pid == 0 {
        return Err(DbgFlowError::Backend(
            "TTD attach pid must be greater than zero".to_string(),
        ));
    }
    Ok(TtdTarget::Attach { pid })
}

fn validate_monitor_target(program: PathBuf, cmd_line_filter: Option<String>) -> Result<TtdTarget> {
    let program_text = program.as_os_str().to_string_lossy();
    validate_plain_text(&program_text, "TTD monitor program")?;
    if program_text.trim().is_empty() {
        return Err(DbgFlowError::Backend(
            "TTD monitor program must not be empty".to_string(),
        ));
    }
    if has_parent_component(&program) || split_path_text_has_parent(&program_text) {
        return Err(DbgFlowError::Backend(
            "TTD monitor program must not contain path traversal".to_string(),
        ));
    }
    if path_text_has_separator(&program_text) && !is_absolute_path_text(&program) {
        return Err(DbgFlowError::Backend(
            "TTD monitor program must be a file name or an absolute path".to_string(),
        ));
    }
    let program = if is_absolute_path_text(&program) {
        let canonical = program.canonicalize().map_err(|error| {
            DbgFlowError::Backend(format!("invalid TTD monitor program path: {error}"))
        })?;
        if !canonical.is_file() {
            return Err(DbgFlowError::Backend(format!(
                "TTD monitor program path is not a file: {}",
                canonical.display()
            )));
        }
        canonical
    } else {
        program
    };
    if let Some(filter) = &cmd_line_filter {
        validate_plain_text(filter, "TTD monitor cmd_line_filter")?;
        if filter.trim().is_empty() {
            return Err(DbgFlowError::Backend(
                "TTD monitor cmd_line_filter must not be empty".to_string(),
            ));
        }
    }
    Ok(TtdTarget::Monitor {
        program,
        cmd_line_filter,
    })
}
