use crate::http::{run_http, HttpConfig};
use crate::logging::FileLogSink;
use crate::mcp::server_with_data_dir_proxy_and_logger;
use crate::runtime::{
    remove_install_files_target, service_process_options_from_command_line, ServiceInstallConfig,
    ServiceProcessConfig, ServiceUninstallConfig, SERVICE_DESCRIPTION,
};
use dbgflow_core::logging::{LogEvent, LogLevel, LogSink};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, PSID, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY,
};
use windows_sys::Win32::System::SystemServices::{
    DOMAIN_ALIAS_RID_ADMINS, SECURITY_BUILTIN_DOMAIN_RID,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

define_windows_service!(ffi_service_main, service_main);

const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const SERVICE_DELETE_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run_dispatcher(service_name: &str) -> windows_service::Result<()> {
    service_dispatcher::start(service_name, ffi_service_main)
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service_process() {
        log_fallback(&format!("service failed: {error}"));
    }
}

fn run_service_process() -> Result<(), String> {
    let config = service_process_options_from_command_line(std::env::args_os())?;
    run_service(config)
}

fn run_service(config: ServiceProcessConfig) -> Result<(), String> {
    let data_dir = config.app.data_dir.clone();
    let logger: Arc<dyn LogSink> = Arc::new(
        FileLogSink::new(data_dir.join("logs"), 7)
            .map_err(|error| format!("initialize log directory: {error}"))?,
    );
    log(
        &logger,
        LogLevel::Info,
        "service_starting",
        "service starting",
    );

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let status_handle = register_control_handler(&config.service_name, shutdown_tx.clone())?;

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
        server_with_data_dir_proxy_and_logger(data_dir, config.app.proxy.clone(), logger.clone()),
        HttpConfig {
            bind: config.app.bind,
        },
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
        Ok(()) => log(
            &logger,
            LogLevel::Info,
            "service_stopped",
            "service stopped",
        ),
        Err(error) => log(
            &logger,
            LogLevel::Error,
            "service_stopped_with_error",
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
    service_name: &str,
    shutdown_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
) -> Result<ServiceStatusHandle, String> {
    service_control_handler::register(service_name, move |control| match control {
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

fn log(logger: &Arc<dyn LogSink>, level: LogLevel, event: &str, message: &str) {
    logger.log(LogEvent::new(level, "service", event).message(message));
}

fn log_fallback(message: &str) {
    eprintln!("{message}");
}

pub fn install(config: ServiceInstallConfig) -> Result<(), String> {
    if ensure_elevated(&config.normalized_command_args())? {
        return Ok(());
    }

    stop_and_delete_service(&config.service_name, true)?;
    assert_port_available(config.bind)?;

    let source_exe =
        std::env::current_exe().map_err(|error| format!("resolve current executable: {error}"))?;
    if !source_exe.exists() {
        return Err(format!(
            "current executable was not found: {}",
            source_exe.display()
        ));
    }

    let bin_dir = config.bin_dir();
    let data_dir = config.data_dir();
    std::fs::create_dir_all(&bin_dir)
        .map_err(|error| format!("create bin directory {}: {error}", bin_dir.display()))?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|error| format!("create data directory {}: {error}", data_dir.display()))?;

    let installed_exe = config.installed_exe();
    std::fs::copy(&source_exe, &installed_exe).map_err(|error| {
        format!(
            "copy {} to {}: {error}",
            source_exe.display(),
            installed_exe.display()
        )
    })?;
    grant_install_root_acl(&config.install_root)?;
    create_service(&config, &installed_exe)?;
    start_service(&config.service_name)?;
    wait_healthz(config.bind)?;

    println!(
        "Service '{}' is running at http://{}/mcp",
        config.service_name, config.bind
    );
    Ok(())
}

pub fn uninstall(config: ServiceUninstallConfig) -> Result<(), String> {
    if ensure_elevated(&config.normalized_command_args())? {
        return Ok(());
    }

    stop_and_delete_service(&config.service_name, false)?;
    if config.remove_install_files {
        if let Some(bin_dir) = verified_existing_remove_target(&config.install_root)? {
            println!("Removing installed binaries at '{}'...", bin_dir.display());
            std::fs::remove_dir_all(&bin_dir)
                .map_err(|error| format!("remove {}: {error}", bin_dir.display()))?;
        }
    }

    println!(
        "Service '{}' has been uninstalled. Artifacts and logs were left under '{}'.",
        config.service_name,
        config.install_root.join("var").display()
    );
    Ok(())
}

fn verified_existing_remove_target(install_root: &Path) -> Result<Option<PathBuf>, String> {
    let bin_dir = remove_install_files_target(install_root.to_path_buf())?;
    if !bin_dir.exists() {
        return Ok(None);
    }

    let resolved_root = std::fs::canonicalize(install_root).map_err(|error| {
        format!(
            "resolve install root before removing files {}: {error}",
            install_root.display()
        )
    })?;
    let resolved_bin = std::fs::canonicalize(&bin_dir).map_err(|error| {
        format!(
            "resolve install bin directory before removing files {}: {error}",
            bin_dir.display()
        )
    })?;
    if !path_starts_with_case_insensitive(&resolved_bin, &resolved_root) {
        return Err(format!(
            "refusing to remove path outside install root: {}",
            resolved_bin.display()
        ));
    }

    Ok(Some(resolved_bin))
}

fn path_starts_with_case_insensitive(path: &Path, base: &Path) -> bool {
    if path.starts_with(base) {
        return true;
    }

    let path = path.to_string_lossy().to_lowercase();
    let base = base.to_string_lossy().to_lowercase();
    path == base
        || path
            .strip_prefix(&base)
            .is_some_and(|rest| rest.starts_with('\\') || rest.starts_with('/'))
}

fn ensure_elevated(args: &[OsString]) -> Result<bool, String> {
    if is_running_as_administrator()? {
        return Ok(false);
    }

    println!("Administrator privileges are required. Requesting elevation with UAC...");
    let exit_code = run_elevated(args)?;
    if exit_code == 0 {
        Ok(true)
    } else {
        Err(format!(
            "elevated command failed with exit code {exit_code}"
        ))
    }
}

fn is_running_as_administrator() -> Result<bool, String> {
    unsafe {
        let mut admin_group: PSID = std::ptr::null_mut();
        let ok = AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID as u32,
            DOMAIN_ALIAS_RID_ADMINS as u32,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admin_group,
        );
        if ok == 0 {
            return Err(format!(
                "initialize Administrators SID: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut is_member = FALSE;
        let ok = CheckTokenMembership(0, admin_group, &mut is_member);
        FreeSid(admin_group);
        if ok == 0 {
            return Err(format!(
                "check administrator membership: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(is_member != FALSE)
    }
}

fn run_elevated(args: &[OsString]) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|error| format!("resolve current exe: {error}"))?;
    let working_dir =
        std::env::current_dir().map_err(|error| format!("resolve current directory: {error}"))?;
    let parameters = join_windows_command_line(args);

    let verb = wide_null("runas");
    let file = wide_null_os(exe.as_os_str());
    let params = wide_null(&parameters);
    let directory = wide_null_os(working_dir.as_os_str());

    let mut info = unsafe { std::mem::zeroed::<SHELLEXECUTEINFOW>() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = directory.as_ptr();
    info.nShow = SW_SHOWNORMAL;

    let launched = unsafe { ShellExecuteExW(&mut info) };
    if launched == 0 {
        return Err(format!(
            "elevation was cancelled or failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    if info.hProcess == 0 {
        return Err("elevated process handle was not returned".to_string());
    }

    let wait_result = unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
    if wait_result == WAIT_FAILED {
        unsafe {
            CloseHandle(info.hProcess);
        }
        return Err(format!(
            "wait for elevated process: {}",
            std::io::Error::last_os_error()
        ));
    }
    if wait_result != WAIT_OBJECT_0 {
        unsafe {
            CloseHandle(info.hProcess);
        }
        return Err(format!(
            "unexpected elevated process wait result: {wait_result}"
        ));
    }

    let mut exit_code = 1;
    let ok = unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) };
    unsafe {
        CloseHandle(info.hProcess);
    }
    if ok == 0 {
        return Err(format!(
            "read elevated process exit code: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(exit_code)
}

fn stop_and_delete_service(service_name: &str, quiet_if_missing: bool) -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| format!("open service manager: {error}"))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let Some(service) = open_service_optional(&manager, service_name, access)? else {
        if !quiet_if_missing {
            println!("Service '{service_name}' is not installed.");
        }
        return Ok(());
    };

    let status = service
        .query_status()
        .map_err(|error| format!("query service '{service_name}' status: {error}"))?;
    if status.current_state != ServiceState::Stopped {
        println!("Stopping service '{service_name}'...");
        service
            .stop()
            .map_err(|error| format!("stop service '{service_name}': {error}"))?;
        wait_service_stopped(&service, service_name, SERVICE_STOP_TIMEOUT)?;
    }

    println!("Deleting service '{service_name}'...");
    service
        .delete()
        .map_err(|error| format!("delete service '{service_name}': {error}"))?;
    drop(service);
    wait_service_deleted(&manager, service_name, SERVICE_DELETE_TIMEOUT)
}

fn open_service_optional(
    manager: &ServiceManager,
    service_name: &str,
    access: ServiceAccess,
) -> Result<Option<windows_service::service::Service>, String> {
    match manager.open_service(service_name, access) {
        Ok(service) => Ok(Some(service)),
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("open service '{service_name}': {error}")),
    }
}

fn wait_service_stopped(
    service: &windows_service::service::Service,
    service_name: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = service
            .query_status()
            .map_err(|error| format!("query service '{service_name}' status: {error}"))?;
        if status.current_state == ServiceState::Stopped {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for service '{service_name}' to stop"
            ));
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn wait_service_deleted(
    manager: &ServiceManager,
    service_name: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if open_service_optional(manager, service_name, ServiceAccess::QUERY_STATUS)?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for service '{service_name}' to be deleted"
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn assert_port_available(bind: SocketAddr) -> Result<(), String> {
    if !bind.ip().is_loopback() {
        return Err(format!(
            "bind address must be loopback; dbgflow does not support remote HTTP access: {bind}"
        ));
    }
    let listener = TcpListener::bind(bind)
        .map_err(|error| format!("bind address {bind} is not available: {error}"))?;
    drop(listener);
    Ok(())
}

fn grant_install_root_acl(install_root: &Path) -> Result<(), String> {
    let output = Command::new("icacls.exe")
        .arg(install_root)
        .arg("/grant")
        .arg("SYSTEM:(OI)(CI)F")
        .arg("Administrators:(OI)(CI)F")
        .arg("/T")
        .output()
        .map_err(|error| format!("start icacls.exe for {}: {error}", install_root.display()))?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "icacls failed for {}: {}{}",
        install_root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn create_service(config: &ServiceInstallConfig, installed_exe: &Path) -> Result<(), String> {
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager = ServiceManager::local_computer(None::<&str>, manager_access)
        .map_err(|error| format!("open service manager: {error}"))?;

    let service_info = ServiceInfo {
        name: OsString::from(&config.service_name),
        display_name: OsString::from(&config.display_name),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: installed_exe.to_path_buf(),
        launch_arguments: config.service_launch_arguments(),
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    println!("Creating service '{}'...", config.service_name);
    let service_access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = manager
        .create_service(&service_info, service_access)
        .map_err(|error| format!("create service '{}': {error}", config.service_name))?;
    service
        .set_description(SERVICE_DESCRIPTION)
        .map_err(|error| format!("set service '{}' description: {error}", config.service_name))?;
    Ok(())
}

fn start_service(service_name: &str) -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| format!("open service manager: {error}"))?;
    let service = manager
        .open_service(
            service_name,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(|error| format!("open service '{service_name}': {error}"))?;
    println!("Starting service '{service_name}'...");
    service
        .start::<&std::ffi::OsStr>(&[])
        .map_err(|error| format!("start service '{service_name}': {error}"))?;
    Ok(())
}

fn wait_healthz(bind: SocketAddr) -> Result<(), String> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        if http_get_healthz(bind).is_ok_and(|healthy| healthy) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "service did not pass health check at http://{bind}/healthz"
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn http_get_healthz(bind: SocketAddr) -> std::io::Result<bool> {
    let mut stream = TcpStream::connect(bind)?;
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return Ok(false);
    };
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    Ok(status == Some(200) && body.contains("\"status\":\"ok\""))
}

fn join_windows_command_line(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_windows_arg(arg: &std::ffi::OsStr) -> String {
    let value = arg.to_string_lossy();
    if value.is_empty() {
        return "\"\"".to_string();
    }
    if !value
        .chars()
        .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '"'))
    {
        return value.into_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_null_os(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}
