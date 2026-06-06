use dbgflow_mcp::http::{run_http, HttpConfig};
use dbgflow_mcp::mcp::server_with_data_dir;
use dbgflow_mcp::runtime::{
    help_text, parse_options, parse_service_install_options, parse_service_uninstall_options,
};
use std::ffi::OsString;
use std::io::{self, BufReader};
use std::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandMode {
    Http,
    ServiceRun,
    ServiceInstall,
    ServiceUninstall,
    WorkerSession,
    Help,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("dbgflow-mcp server error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    match parse_command_mode(args.clone().into_iter())? {
        CommandMode::WorkerSession => {
            run_session_worker().map_err(|error| format!("session worker error: {error}"))
        }
        CommandMode::Http => {
            let config = parse_options(args.into_iter().skip(1))?;
            let (_shutdown_tx, shutdown_rx) = mpsc::channel();
            let server = server_with_data_dir(config.data_dir)?;
            run_http(server, HttpConfig { bind: config.bind }, shutdown_rx)
                .map_err(|error| error.to_string())
        }
        CommandMode::ServiceRun => run_service(args.into_iter().skip(2)),
        CommandMode::ServiceInstall => {
            let config = parse_service_install_options(args.into_iter().skip(2))?;
            install_service(config)
        }
        CommandMode::ServiceUninstall => {
            let config = parse_service_uninstall_options(args.into_iter().skip(2))?;
            uninstall_service(config)
        }
        CommandMode::Help => {
            println!("{}", help_text());
            Ok(())
        }
    }
}

fn parse_command_mode<I>(args: I) -> Result<CommandMode, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{}", help_text()));
    };
    let command = command
        .into_string()
        .map_err(|_| "command must be valid UTF-8".to_string())?;

    match command.as_str() {
        dbgflow_core::session::worker::SESSION_WORKER_COMMAND => {
            let Some(kind) = args.next() else {
                return Err("missing worker kind".to_string());
            };
            let kind = kind
                .into_string()
                .map_err(|_| "worker kind must be valid UTF-8".to_string())?;
            if kind != dbgflow_core::session::worker::SESSION_WORKER_KIND_SESSION {
                return Err(format!("unknown worker kind: {kind}"));
            }
            if args.next().is_some() {
                return Err("worker session does not accept extra arguments".to_string());
            }
            Ok(CommandMode::WorkerSession)
        }
        "http" => Ok(CommandMode::Http),
        "service" => {
            let Some(next) = args.next() else {
                return Err(format!("missing service command\n\n{}", help_text()));
            };
            let next = next
                .into_string()
                .map_err(|_| "service command must be valid UTF-8".to_string())?;
            match next.as_str() {
                "run" => Ok(CommandMode::ServiceRun),
                "install" => Ok(CommandMode::ServiceInstall),
                "uninstall" => Ok(CommandMode::ServiceUninstall),
                other => Err(format!(
                    "unknown service command: {other}\n\n{}",
                    help_text()
                )),
            }
        }
        "--help" | "-h" | "help" => Ok(CommandMode::Help),
        other => Err(format!("unknown command: {other}\n\n{}", help_text())),
    }
}

fn run_session_worker() -> io::Result<()> {
    let stdin = io::stdin();
    dbgflow_core::session::worker::run_session_worker_stdio(
        BufReader::new(stdin.lock()),
        io::stdout(),
    )
}

#[cfg(windows)]
fn run_service(args: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let config = dbgflow_mcp::runtime::parse_service_process_options(args)?;
    dbgflow_mcp::service::run_dispatcher(&config.service_name).map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn run_service(_args: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    Err("Windows service mode is only supported on Windows".to_string())
}

#[cfg(windows)]
fn install_service(config: dbgflow_mcp::runtime::ServiceInstallConfig) -> Result<(), String> {
    dbgflow_mcp::service::install(config)
}

#[cfg(not(windows))]
fn install_service(_config: dbgflow_mcp::runtime::ServiceInstallConfig) -> Result<(), String> {
    Err("Windows service install is only supported on Windows".to_string())
}

#[cfg(windows)]
fn uninstall_service(config: dbgflow_mcp::runtime::ServiceUninstallConfig) -> Result<(), String> {
    dbgflow_mcp::service::uninstall(config)
}

#[cfg(not(windows))]
fn uninstall_service(_config: dbgflow_mcp::runtime::ServiceUninstallConfig) -> Result<(), String> {
    Err("Windows service uninstall is only supported on Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::{parse_command_mode, CommandMode};
    use std::ffi::OsString;

    #[test]
    fn parses_public_http_command() {
        let args = [OsString::from("http")].into_iter();
        assert_eq!(parse_command_mode(args), Ok(CommandMode::Http));
    }

    #[test]
    fn parses_internal_worker_session_command() {
        let args = [OsString::from("worker"), OsString::from("session")].into_iter();
        assert_eq!(parse_command_mode(args), Ok(CommandMode::WorkerSession));
    }

    #[test]
    fn parses_service_management_commands() {
        let run_args = [OsString::from("service"), OsString::from("run")].into_iter();
        assert_eq!(parse_command_mode(run_args), Ok(CommandMode::ServiceRun));

        let install_args = [OsString::from("service"), OsString::from("install")].into_iter();
        assert_eq!(
            parse_command_mode(install_args),
            Ok(CommandMode::ServiceInstall)
        );

        let uninstall_args = [OsString::from("service"), OsString::from("uninstall")].into_iter();
        assert_eq!(
            parse_command_mode(uninstall_args),
            Ok(CommandMode::ServiceUninstall)
        );
    }

    #[test]
    fn rejects_service_without_action_subcommand() {
        let args = [OsString::from("service")].into_iter();
        let error = parse_command_mode(args).expect_err("reject missing service action");
        assert!(error.contains("missing service command"));
    }

    #[test]
    fn rejects_legacy_service_runtime_without_run_subcommand() {
        let args = [
            OsString::from("service"),
            OsString::from("--data-dir"),
            OsString::from("C:\\dbgflow\\var"),
        ]
        .into_iter();
        let error = parse_command_mode(args).expect_err("reject legacy service runtime");
        assert!(error.contains("unknown service command"));
    }

    #[test]
    fn rejects_legacy_hidden_worker_command() {
        let args = [OsString::from("__dbgflow-session-worker")].into_iter();
        let error = parse_command_mode(args).expect_err("reject hidden command");
        assert!(error.contains("unknown command"));
    }

    #[test]
    fn rejects_worker_session_extra_arguments() {
        let args = [
            OsString::from("worker"),
            OsString::from("session"),
            OsString::from("extra"),
        ]
        .into_iter();
        let error = parse_command_mode(args).expect_err("reject extra argument");
        assert!(error.contains("extra"));
    }
}
