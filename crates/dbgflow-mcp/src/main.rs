use dbgflow_mcp::http::{run_http, HttpConfig};
use dbgflow_mcp::mcp::{default_server, run_stdio, server_with_data_dir};
use dbgflow_mcp::runtime::{help_text, parse_options};
use std::io::{self, BufReader};
use std::sync::mpsc;

fn main() {
    if let Err(error) = run() {
        eprintln!("dbgflow-mcp server error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os();
    let _exe = args.next();
    let Some(command) = args.next() else {
        return run_stdio_server().map_err(|error| error.to_string());
    };
    let command = command
        .into_string()
        .map_err(|_| "command must be valid UTF-8".to_string())?;

    match command.as_str() {
        "http" => {
            let config = parse_options(args)?;
            let (_shutdown_tx, shutdown_rx) = mpsc::channel();
            let server = match config.data_dir {
                Some(data_dir) => server_with_data_dir(data_dir)?,
                None => default_server(),
            };
            run_http(server, HttpConfig { bind: config.bind }, shutdown_rx)
                .map_err(|error| error.to_string())
        }
        "service" => run_service(args),
        "--help" | "-h" | "help" => {
            println!("{}", help_text());
            Ok(())
        }
        other => Err(format!("unknown command: {other}\n\n{}", help_text())),
    }
}

fn run_stdio_server() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    run_stdio(
        default_server(),
        BufReader::new(stdin.lock()),
        stdout.lock(),
    )
}

#[cfg(windows)]
fn run_service(_args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    dbgflow_mcp::service::run_dispatcher().map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn run_service(_args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    Err("Windows service mode is only supported on Windows".to_string())
}
