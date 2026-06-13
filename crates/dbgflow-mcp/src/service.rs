use crate::http::{run_http, HttpConfig};
use crate::logging::FileLogSink;
use crate::mcp::server_with_data_dir_proxy_ttd_symbol_path_and_logger;
use crate::runtime::{
    apply_runtime_environment, parse_installed_service_command,
    service_process_options_from_command_line, validate_install_root_removal, RuntimeConfig,
    ServiceInstallConfig, ServiceProcessConfig, ServiceUninstallConfig, SERVICE_DESCRIPTION,
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
        eprintln!("service failed: {error}");
    }
}

fn run_service_process() -> Result<(), String> {
    let config = service_process_options_from_command_line(std::env::args_os())?;
    run_service(config)
}

fn run_service(config: ServiceProcessConfig) -> Result<(), String> {
    apply_runtime_environment(&config.app);
    let data_dir = config.app.data_dir.clone();
    let logger: Arc<dyn LogSink> = Arc::new(
        FileLogSink::new(data_dir.join("logs"), 7)
            .map_err(|error| format!("initialize log directory: {error}"))?,
    );
    logger.log(
        LogEvent::new(LogLevel::Info, "service", "service_starting")
            .message("service starting")
            .field("service_name", config.service_name.clone())
            .field("bind", config.app.bind.to_string())
            .field("data_dir", config.app.data_dir.display().to_string())
            .field("proxy_source", format!("{:?}", config.app.proxy.source()))
            .field(
                "ttd_dir",
                config
                    .app
                    .ttd_dir
                    .as_ref()
                    .map(|path| path.display().to_string()),
            )
            .field(
                "dbgeng_dir",
                config
                    .app
                    .dbgeng_dir
                    .as_ref()
                    .map(|path| path.display().to_string()),
            )
            .field("symbol_path_configured", config.app.symbol_path.is_some()),
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
        server_with_data_dir_proxy_ttd_symbol_path_and_logger(
            data_dir,
            config.app.proxy.clone(),
            config.app.ttd_dir.clone(),
            config.app.symbol_path.clone(),
            logger.clone(),
        ),
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
        Ok(()) => logger.log(
            LogEvent::new(LogLevel::Info, "service", "service_stopped")
                .message("service stopped")
                .field("service_name", config.service_name.clone()),
        ),
        Err(error) => logger.log(
            LogEvent::new(LogLevel::Error, "service", "service_stopped_with_error")
                .message(format!("service stopped with error: {error}"))
                .field("service_name", config.service_name.clone())
                .error(error.clone()),
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

pub fn install(config: ServiceInstallConfig) -> Result<(), String> {
    if ensure_elevated(&config.normalized_command_args())? {
        return Ok(());
    }

    stop_and_delete_service(&config.service.name, true)?;
    assert_port_available(config.app.bind)?;

    let source_exe =
        std::env::current_exe().map_err(|error| format!("resolve current executable: {error}"))?;
    if !source_exe.exists() {
        return Err(format!(
            "current executable was not found: {}",
            source_exe.display()
        ));
    }

    let bin_dir = config.bin_dir();
    let data_dir = config.app.data_dir.clone();
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
    grant_install_root_acl(&config.service.install_root)?;
    create_service(&config, &installed_exe)?;
    start_service(&config.service.name)?;
    wait_healthz(config.app.bind)?;

    println!(
        "Service '{}' is running at http://{}/mcp",
        config.service.name, config.app.bind
    );
    Ok(())
}

pub fn uninstall(request: ServiceUninstallConfig) -> Result<(), String> {
    if ensure_elevated(&request.normalized_command_args())? {
        return Ok(());
    }

    let plan = resolve_uninstall_plan(&request)?;
    if plan.service_exists {
        stop_and_delete_service(&plan.service_name, false)?;
    } else {
        println!("Service '{}' is not installed.", plan.service_name);
    }

    if plan.install_root.exists() {
        println!("Removing install root '{}'...", plan.install_root.display());
        std::fs::remove_dir_all(&plan.install_root)
            .map_err(|error| format!("remove {}: {error}", plan.install_root.display()))?;
    }

    println!(
        "Service '{}' has been uninstalled and install root '{}' was removed.",
        plan.service_name,
        plan.install_root.display()
    );
    Ok(())
}

struct UninstallPlan {
    service_name: String,
    install_root: PathBuf,
    service_exists: bool,
}

fn resolve_uninstall_plan(request: &ServiceUninstallConfig) -> Result<UninstallPlan, String> {
    match installed_service_command(&request.service_name) {
        Ok(Some(command)) => {
            if let Some(explicit) = &request.config_path {
                let explicit = normalize_existing_config(explicit)?;
                let installed = normalize_existing_config(&command.config_path)?;
                if !path_eq_case_insensitive(&explicit, &installed) {
                    return Err(format!(
                        "--config {} does not match installed service config {}",
                        explicit.display(),
                        installed.display()
                    ));
                }
            }
            uninstall_plan_from_config(
                &request.service_name,
                &command.config_path,
                Some(&command.executable_path),
                true,
            )
        }
        Ok(None) => {
            let Some(config_path) = &request.config_path else {
                return Err(format!(
                    "service '{}' is not installed and --config was not provided",
                    request.service_name
                ));
            };
            uninstall_plan_from_config(&request.service_name, config_path, None, false)
        }
        Err(error) => {
            let Some(config_path) = &request.config_path else {
                return Err(error);
            };
            eprintln!(
                "Could not read installed service config for '{}': {error}. Falling back to explicit --config.",
                request.service_name
            );
            uninstall_plan_from_config(&request.service_name, config_path, None, true)
        }
    }
}

fn uninstall_plan_from_config(
    service_name: &str,
    config_path: &Path,
    installed_exe: Option<&Path>,
    service_exists: bool,
) -> Result<UninstallPlan, String> {
    let config = RuntimeConfig::load(config_path)?;
    if config.service.name != service_name {
        return Err(format!(
            "config service.name '{}' does not match service '{}'",
            config.service.name, service_name
        ));
    }
    let install_root = validate_install_root_removal(&config, installed_exe)?;
    Ok(UninstallPlan {
        service_name: service_name.to_string(),
        install_root,
        service_exists,
    })
}

fn installed_service_command(
    service_name: &str,
) -> Result<Option<crate::runtime::InstalledServiceCommand>, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| format!("open service manager: {error}"))?;
    let Some(service) = open_service_optional(&manager, service_name, ServiceAccess::QUERY_CONFIG)?
    else {
        return Ok(None);
    };
    let service_config = service
        .query_config()
        .map_err(|error| format!("query service '{service_name}' config: {error}"))?;
    let command_line = service_config.executable_path.to_string_lossy();
    parse_installed_service_command(&command_line).map(Some).map_err(|error| {
        format!(
            "parse service '{service_name}' command line '{}': {error}; pass --config <path> to uninstall with an explicit config",
            command_line
        )
    })
}

fn normalize_existing_config(path: &Path) -> Result<PathBuf, String> {
    RuntimeConfig::load(path).map(|config| config.config_path)
}

fn path_eq_case_insensitive(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
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
                "initialize administrator SID: {}",
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
        name: OsString::from(&config.service.name),
        display_name: OsString::from(&config.service.display_name),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: installed_exe.to_path_buf(),
        launch_arguments: config.service_launch_arguments(),
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    println!("Creating service '{}'...", config.service.name);
    let service_access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = manager
        .create_service(&service_info, service_access)
        .map_err(|error| format!("create service '{}': {error}", config.service.name))?;
    service
        .set_description(SERVICE_DESCRIPTION)
        .map_err(|error| format!("set service '{}' description: {error}", config.service.name))?;
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
        .start::<OsString>(&[])
        .map_err(|error| format!("start service '{service_name}': {error}"))?;
    Ok(())
}

fn wait_healthz(bind: SocketAddr) -> Result<(), String> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    while Instant::now() < deadline {
        if http_get_healthz(bind).is_ok_and(|healthy| healthy) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "service did not pass health check at http://{bind}/healthz"
    ))
}

fn http_get_healthz(bind: SocketAddr) -> std::io::Result<bool> {
    let mut stream = TcpStream::connect(bind)?;
    stream.write_all(
        format!("GET /healthz HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n").as_bytes(),
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

#[cfg(test)]
mod tests {
    use super::{join_windows_command_line, parse_installed_service_command};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn service_launch_command_round_trips_through_parser() {
        let args = vec![
            OsString::from(r"C:\Users\dstars\AppData\Local\dbgflow\bin\dbgflow-mcp.exe"),
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--config"),
            OsString::from(r"C:\Users\dstars\AppData\Local\dbgflow\config.toml"),
        ];
        let command = join_windows_command_line(&args);
        let parsed = parse_installed_service_command(&command).expect("parse service command");

        assert_eq!(
            parsed.executable_path,
            PathBuf::from(r"C:\Users\dstars\AppData\Local\dbgflow\bin\dbgflow-mcp.exe")
        );
        assert_eq!(
            parsed.config_path,
            PathBuf::from(r"C:\Users\dstars\AppData\Local\dbgflow\config.toml")
        );
    }
}
