use super::{TtdRecordMode, TtdRecordingOptions, TtdReplayCpuSupport, TtdTarget};
use std::ffi::OsString;
use std::path::Path;

pub fn build_ttd_args(
    target: &TtdTarget,
    options: &TtdRecordingOptions,
    traces_dir: &Path,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-out"),
        traces_dir.as_os_str().to_os_string(),
    ];
    if options.no_ui {
        args.push(OsString::from("-noUI"));
    }
    if options.accept_eula {
        args.push(OsString::from("-accepteula"));
    }
    if options.children {
        args.push(OsString::from("-children"));
    }
    if options.ring {
        args.push(OsString::from("-ring"));
    }
    args.push(OsString::from("-maxFile"));
    args.push(OsString::from(options.max_file_mb.to_string()));
    for module in &options.modules {
        args.push(OsString::from("-module"));
        args.push(OsString::from(module));
    }
    if options.record_mode != TtdRecordMode::Automatic {
        args.push(OsString::from("-recordmode"));
        args.push(OsString::from(options.record_mode.as_ttd_arg()));
    }
    if options.replay_cpu_support != TtdReplayCpuSupport::Default {
        args.push(OsString::from("-replayCpuSupport"));
        args.push(OsString::from(options.replay_cpu_support.as_ttd_arg()));
    }

    match target {
        TtdTarget::Launch {
            executable,
            args: target_args,
        } => {
            args.push(OsString::from("-launch"));
            args.push(executable.as_os_str().to_os_string());
            args.extend(target_args.iter().map(OsString::from));
        }
        TtdTarget::Attach { pid } => {
            args.push(OsString::from("-attach"));
            args.push(OsString::from(pid.to_string()));
        }
        TtdTarget::Monitor {
            program,
            cmd_line_filter,
        } => {
            if let Some(filter) = cmd_line_filter {
                args.push(OsString::from("-cmdLineFilter"));
                args.push(OsString::from(filter));
            }
            args.push(OsString::from("-monitor"));
            args.push(program.as_os_str().to_os_string());
        }
    }
    args
}

pub(super) fn ttd_stop_target(target: &TtdTarget, recorded_pid: Option<u32>) -> Option<OsString> {
    match target {
        TtdTarget::Monitor { .. } => Some(OsString::from("all")),
        TtdTarget::Attach { pid } => Some(OsString::from(recorded_pid.unwrap_or(*pid).to_string())),
        TtdTarget::Launch { executable, .. } => recorded_pid
            .map(|pid| OsString::from(pid.to_string()))
            .or_else(|| executable.file_name().map(OsString::from)),
    }
}
