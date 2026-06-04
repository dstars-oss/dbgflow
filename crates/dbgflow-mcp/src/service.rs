use crate::http::{run_http, HttpConfig};
use crate::mcp::server_with_artifact_root;
use crate::runtime::{parse_options, SERVICE_NAME};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;

define_windows_service!(ffi_service_main, service_main);

pub fn run_dispatcher() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        log_fallback(&format!("service failed: {error}"));
    }
}

fn run_service() -> Result<(), String> {
    let args = std::env::args_os().skip(2);
    let config = parse_options(args)?;
    log(&config.log_dir, "service starting");

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let status_handle = register_control_handler(shutdown_tx.clone())?;

    set_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        false,
    )?;
    set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
        false,
    )?;

    let result = run_http(
        server_with_artifact_root(config.artifact_root.clone()),
        HttpConfig { bind: config.bind },
        shutdown_rx,
    )
    .map_err(|error| error.to_string());

    set_status(
        &status_handle,
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        1,
        false,
    )?;
    match &result {
        Ok(()) => log(&config.log_dir, "service stopped"),
        Err(error) => log(
            &config.log_dir,
            &format!("service stopped with error: {error}"),
        ),
    }
    set_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        0,
        result.is_err(),
    )?;

    result
}

fn register_control_handler(
    shutdown_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
) -> Result<ServiceStatusHandle, String> {
    service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Ok(mut sender) = shutdown_tx.lock() {
                if let Some(sender) = sender.take() {
                    let _ = sender.send(());
                }
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .map_err(|error| error.to_string())
}

fn set_status(
    status_handle: &ServiceStatusHandle,
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    checkpoint: u32,
    failed: bool,
) -> Result<(), String> {
    status_handle
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(if failed { 1 } else { 0 }),
            checkpoint,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
        .map_err(|error| error.to_string())
}

fn log(log_dir: &Option<std::path::PathBuf>, message: &str) {
    if let Some(log_dir) = log_dir {
        let _ = append_log(log_dir, message);
    }
}

fn append_log(log_dir: &Path, message: &str) -> std::io::Result<()> {
    fs::create_dir_all(log_dir)?;
    let path = log_dir.join("service.log");
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{} {message}", now_unix_ms())
}

fn log_fallback(message: &str) {
    eprintln!("{message}");
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
