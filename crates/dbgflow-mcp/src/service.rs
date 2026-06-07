use crate::http::{run_http, HttpConfig};
use crate::logging::FileLogSink;
use crate::mcp::server_with_data_dir_proxy_sysinternals_and_logger;
use crate::runtime::{
    dbgeng_dir_from_dependency_root, remove_install_files_target,
    service_process_options_from_command_line, ServiceInstallConfig, ServiceProcessConfig,
    ServiceUninstallConfig, SERVICE_DESCRIPTION,
};
use dbgflow_core::backend::dbgeng::DBGFLOW_DBGENG_DIR_ENV;
use dbgflow_core::logging::{LogEvent, LogLevel, LogSink};
use dbgflow_core::proxy::{ProxyEnvironment, ProxySource};
use std::ffi::OsString;
use std::io::{self, Read, Write};
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
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_MULTI_SZ,
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
const ERROR_SUCCESS: u32 = 0;

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
        server_with_data_dir_proxy_sysinternals_and_logger(
            data_dir,
            config.app.proxy.clone(),
            config.app.sysinternals_dir.clone(),
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

pub fn install(mut config: ServiceInstallConfig) -> Result<(), String> {
    if config.interactive {
        config = run_install_journey(config)?;
    }

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
    set_service_environment(&config)?;
    start_service(&config.service_name)?;
    wait_healthz(config.bind)?;

    println!(
        "Service '{}' is running at http://{}/mcp",
        config.service_name, config.bind
    );
    Ok(())
}

fn run_install_journey(mut config: ServiceInstallConfig) -> Result<ServiceInstallConfig, String> {
    println!("dbgflow Windows service install");
    println!("Press Enter to accept a value, or type a replacement.");

    config.service_name = prompt_string("Service name", &config.service_name, |value| {
        validate_service_name(value)
    })?;
    config.display_name = prompt_string("Display name", &config.display_name, |value| {
        if value.trim().is_empty() {
            Err("display name must not be empty".to_string())
        } else {
            Ok(())
        }
    })?;
    config.bind = prompt_string("Bind address", &config.bind.to_string(), |value| {
        value
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid bind address {value}: {error}"))
            .and_then(|bind| {
                if bind.ip().is_loopback() {
                    Ok(())
                } else {
                    Err("bind address must be loopback".to_string())
                }
            })
    })?
    .parse()
    .map_err(|error| format!("invalid bind address: {error}"))?;

    let install_root_default = config.install_root.display().to_string();
    config.install_root = PathBuf::from(prompt_string(
        "Install root",
        &install_root_default,
        |value| {
            if value.trim().is_empty() {
                Err("install root must not be empty".to_string())
            } else {
                Ok(())
            }
        },
    )?);

    config.dbgeng_dir = prompt_dbgeng_dir(config.dbgeng_dir)?;
    config.sysinternals_dir = prompt_sysinternals_dir(config.sysinternals_dir)?;
    config.proxy = prompt_proxy(config.proxy)?;

    print_install_summary(&config);
    if !prompt_yes_no("Install service with these settings?", true)? {
        return Err("service install cancelled".to_string());
    }
    config.interactive = false;
    Ok(config)
}

fn prompt_string(
    label: &str,
    default_value: &str,
    validate: impl Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
    loop {
        let input = read_prompt(&format!("{label} [{default_value}]: "))?;
        let value = if input.trim().is_empty() {
            default_value.to_string()
        } else {
            input.trim().to_string()
        };
        match validate(&value) {
            Ok(()) => return Ok(value),
            Err(error) => println!("{error}"),
        }
    }
}

fn prompt_dbgeng_dir(current: Option<PathBuf>) -> Result<Option<PathBuf>, String> {
    let detected = current.or_else(find_dbgeng_dir);
    if let Some(path) = &detected {
        println!("Detected DbgEng directory: {}", path.display());
    } else {
        println!("DbgEng directory was not detected; System32 fallback may still work.");
    }
    prompt_optional_path(
        "DbgEng directory containing dbgeng.dll",
        detected,
        |path| dbgeng_dir_from_dependency_root(path),
        "directory must contain dbgeng.dll or a Debuggers\\<arch>\\dbgeng.dll child",
    )
}

fn prompt_sysinternals_dir(current: Option<PathBuf>) -> Result<Option<PathBuf>, String> {
    let detected = current.or_else(find_sysinternals_dir);
    if let Some(path) = &detected {
        println!("Detected Sysinternals directory: {}", path.display());
    } else {
        println!("Sysinternals directory was not detected; Procmon features will be unavailable.");
    }
    prompt_optional_path(
        "Sysinternals directory containing Procmon64.exe or Procmon.exe",
        detected,
        |path| {
            if is_sysinternals_dir(path) {
                Some(path.to_path_buf())
            } else {
                None
            }
        },
        "directory must contain Procmon64.exe or Procmon.exe",
    )
}

fn prompt_optional_path(
    label: &str,
    default_value: Option<PathBuf>,
    resolve: impl Fn(&Path) -> Option<PathBuf>,
    error: &str,
) -> Result<Option<PathBuf>, String> {
    let default_text = default_value
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "none".to_string());
    loop {
        let input = read_prompt(&format!("{label} [{default_text}]: "))?;
        let trimmed = input.trim();
        if trimmed.eq_ignore_ascii_case("none") || trimmed.eq_ignore_ascii_case("skip") {
            return Ok(None);
        }
        let Some(candidate) = (if trimmed.is_empty() {
            default_value.clone()
        } else {
            Some(PathBuf::from(trimmed))
        }) else {
            return Ok(None);
        };
        if let Some(resolved) = resolve(&candidate) {
            return Ok(Some(canonicalize_if_possible(resolved)));
        }
        println!("{error}: {}", candidate.display());
    }
}

fn prompt_proxy(current: ProxyEnvironment) -> Result<ProxyEnvironment, String> {
    let default_label = proxy_prompt_default_label(&current);
    println!("Leave proxy blank to keep the displayed value. Type 'none' to clear known proxy variables for the service.");
    loop {
        let input = read_prompt(&format!("Proxy URL [{default_label}]: "))?;
        let trimmed = input.trim();
        if trimmed.eq_ignore_ascii_case("none") || trimmed.eq_ignore_ascii_case("skip") {
            return Ok(ProxyEnvironment::disabled());
        }
        if trimmed.is_empty() {
            return Ok(current.clone());
        }
        match ProxyEnvironment::from_cli_proxy_url(trimmed) {
            Ok(proxy) => return Ok(proxy),
            Err(error) => println!("{error}"),
        }
    }
}

fn proxy_prompt_default_label(current: &ProxyEnvironment) -> String {
    match current.source() {
        ProxySource::None => "not configured".to_string(),
        ProxySource::Disabled => "none".to_string(),
        ProxySource::Cli | ProxySource::Environment => current
            .value_for("HTTP_PROXY")
            .or_else(|| {
                current
                    .value_for("_NT_SYMBOL_PROXY")
                    .map(|value| format!("_NT_SYMBOL_PROXY={value}"))
            })
            .unwrap_or_else(|| "configured".to_string()),
    }
}

fn print_install_summary(config: &ServiceInstallConfig) {
    println!();
    println!("Service name: {}", config.service_name);
    println!("Display name: {}", config.display_name);
    println!("Bind: {}", config.bind);
    println!("Install root: {}", config.install_root.display());
    println!("Data dir: {}", config.data_dir().display());
    println!(
        "DbgEng dir: {}",
        config
            .dbgeng_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not configured".to_string())
    );
    println!(
        "Sysinternals dir: {}",
        config
            .sysinternals_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not configured".to_string())
    );
    println!("Proxy: {:?}", config.proxy);
    println!(
        "Service command: {} {}",
        config.installed_exe().display(),
        join_windows_command_line(&config.service_launch_arguments())
    );
    println!();
}

fn prompt_yes_no(label: &str, default_yes: bool) -> Result<bool, String> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    loop {
        let input = read_prompt(&format!("{label} {suffix}: "))?;
        let value = input.trim();
        if value.is_empty() {
            return Ok(default_yes);
        }
        if value.eq_ignore_ascii_case("y") || value.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        if value.eq_ignore_ascii_case("n") || value.eq_ignore_ascii_case("no") {
            return Ok(false);
        }
        println!("Please answer y or n.");
    }
}

fn read_prompt(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|error| format!("write prompt: {error}"))?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| format!("read prompt: {error}"))?;
    Ok(input.trim_end_matches(['\r', '\n']).to_string())
}

fn find_dbgeng_dir() -> Option<PathBuf> {
    find_dbgeng_dir_from_roots(default_dbgeng_search_roots())
}

fn default_dbgeng_search_roots() -> DbgEngSearchRoots {
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));

    let mut sdk_roots = Vec::new();
    push_env_path(&mut sdk_roots, "WindowsSdkDir");
    push_env_path(&mut sdk_roots, "WDKContentRoot");
    push_env_path(&mut sdk_roots, "WindowsSDK_ExecutablePath_x64");
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from) {
        sdk_roots.push(program_files_x86.join("Windows Kits").join("10"));
    }
    sdk_roots.push(program_files.join("Windows Kits").join("10"));
    sdk_roots.push(PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10"));
    sdk_roots.push(PathBuf::from(r"C:\Program Files\Windows Kits\10"));

    DbgEngSearchRoots {
        app_store_root: program_files.join("WindowsApps"),
        sdk_roots,
        system_root,
    }
}

fn find_dbgeng_dir_from_roots(roots: DbgEngSearchRoots) -> Option<PathBuf> {
    find_app_store_dbgeng_dir(&roots.app_store_root)
        .or_else(|| {
            first_resolved_candidate(roots.sdk_roots, |path| {
                dbgeng_dir_from_dependency_root(path)
            })
        })
        .or_else(|| {
            let system32 = roots.system_root.join("System32");
            system32.join("dbgeng.dll").is_file().then_some(system32)
        })
        .map(canonicalize_if_possible)
}

struct DbgEngSearchRoots {
    app_store_root: PathBuf,
    sdk_roots: Vec<PathBuf>,
    system_root: PathBuf,
}

fn find_app_store_dbgeng_dir(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut packages = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Microsoft.WinDbg"))
        })
        .collect::<Vec<_>>();
    packages.sort();
    packages.reverse();

    packages.into_iter().find_map(|package| {
        find_file_limited(&package, "dbgeng.dll", 4)
            .and_then(|path| path.parent().map(Path::to_path_buf))
    })
}

fn find_file_limited(root: &Path, file_name: &str, max_depth: usize) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case(file_name))
        {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_file_limited(&path, file_name, max_depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn find_sysinternals_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    push_env_path(&mut candidates, "DBGFLOW_SYSINTERNALS_DIR");
    push_env_path(&mut candidates, "SysinternalsDir");
    push_path_entries(&mut candidates);
    if let Ok(current_dir) = std::env::current_dir() {
        candidates.push(current_dir.join("Sysinternals"));
        if let Some(parent) = current_dir.parent() {
            candidates.push(parent.join("Sysinternals"));
        }
    }
    candidates.push(PathBuf::from(r"C:\Tools\Sysinternals"));
    candidates.push(PathBuf::from(r"C:\Sysinternals"));
    candidates.push(PathBuf::from(r"C:\Program Files\Sysinternals"));

    first_resolved_candidate(candidates, |path| {
        if is_sysinternals_dir(path) {
            Some(path.to_path_buf())
        } else {
            None
        }
    })
}

fn first_resolved_candidate(
    candidates: Vec<PathBuf>,
    resolve: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let mut seen = Vec::<PathBuf>::new();
    for candidate in candidates {
        if seen
            .iter()
            .any(|seen| path_eq_case_insensitive(seen, &candidate))
        {
            continue;
        }
        seen.push(candidate.clone());
        if let Some(resolved) = resolve(&candidate) {
            return Some(canonicalize_if_possible(resolved));
        }
    }
    None
}

fn push_env_path(candidates: &mut Vec<PathBuf>, key: &str) {
    if let Some(value) = std::env::var_os(key) {
        if !value.is_empty() {
            candidates.push(PathBuf::from(value));
        }
    }
}

fn push_path_entries(candidates: &mut Vec<PathBuf>) {
    let Some(path) = std::env::var_os("PATH") else {
        return;
    };
    candidates.extend(std::env::split_paths(&path));
}

fn is_sysinternals_dir(path: &Path) -> bool {
    path.join("Procmon64.exe").is_file() || path.join("Procmon.exe").is_file()
}

fn canonicalize_if_possible(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn path_eq_case_insensitive(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn validate_service_name(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("service name must not be empty".to_string());
    }
    if value
        .chars()
        .any(|ch| matches!(ch, '/' | '\\' | '*' | '?' | '[' | ']') || ch.is_control())
    {
        return Err(
            "service name must not contain path separators, wildcards, or control characters"
                .to_string(),
        );
    }
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

fn set_service_environment(config: &ServiceInstallConfig) -> Result<(), String> {
    let entries = service_environment_entries(config);
    if entries.is_empty() {
        return Ok(());
    }

    println!(
        "Writing service environment for '{}'...",
        config.service_name
    );
    let key_path = format!(r"SYSTEM\CurrentControlSet\Services\{}", config.service_name);
    let key_path = wide_null(&key_path);
    let mut key = 0;
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key_path.as_ptr(),
            0,
            std::ptr::null_mut(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(format!(
            "open service registry key for '{}': {}",
            config.service_name,
            std::io::Error::from_raw_os_error(result as i32)
        ));
    }

    let name = wide_null("Environment");
    let value = multi_sz(&entries);
    let bytes = unsafe {
        std::slice::from_raw_parts(
            value.as_ptr() as *const u8,
            value.len() * std::mem::size_of::<u16>(),
        )
    };
    let result = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_MULTI_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if result != ERROR_SUCCESS {
        return Err(format!(
            "write service environment for '{}': {}",
            config.service_name,
            std::io::Error::from_raw_os_error(result as i32)
        ));
    }
    Ok(())
}

fn service_environment_entries(config: &ServiceInstallConfig) -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(dbgeng_dir) = &config.dbgeng_dir {
        entries.push(format!(
            "{}={}",
            DBGFLOW_DBGENG_DIR_ENV,
            dbgeng_dir.display()
        ));
    }

    match config.proxy.source() {
        ProxySource::None => {}
        ProxySource::Cli | ProxySource::Environment => {
            entries.extend(
                config
                    .proxy
                    .env_vars()
                    .map(|(key, value)| format!("{key}={value}")),
            );
            entries.extend(
                config
                    .proxy
                    .removed_keys()
                    .into_iter()
                    .map(|key| format!("{key}=")),
            );
        }
        ProxySource::Disabled => {
            entries.extend(
                config
                    .proxy
                    .removed_keys()
                    .into_iter()
                    .map(|key| format!("{key}=")),
            );
        }
    }
    entries
}

fn multi_sz(values: &[String]) -> Vec<u16> {
    let mut output = Vec::new();
    for value in values {
        output.extend(value.encode_utf16());
        output.push(0);
    }
    output.push(0);
    output
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

#[cfg(test)]
mod tests {
    use super::{
        find_dbgeng_dir_from_roots, proxy_prompt_default_label, DbgEngSearchRoots, ProxyEnvironment,
    };
    use std::path::PathBuf;

    #[test]
    fn proxy_prompt_default_preserves_disabled_proxy() {
        assert_eq!(
            proxy_prompt_default_label(&ProxyEnvironment::disabled()),
            "none"
        );
    }

    #[test]
    fn proxy_prompt_default_without_proxy_is_not_configured() {
        assert_eq!(
            proxy_prompt_default_label(&ProxyEnvironment::none()),
            "not configured"
        );
    }

    #[test]
    fn proxy_prompt_default_uses_cli_proxy_when_configured() {
        let proxy =
            ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897").expect("parse proxy");

        assert_eq!(proxy_prompt_default_label(&proxy), "http://127.0.0.1:7897");
    }

    #[test]
    fn dbgeng_install_detection_prefers_app_store_over_sdk_and_system32() {
        let root = unique_test_dir("dbgflow-install-dbgeng-store-order");
        let app_dbgeng = root
            .join("WindowsApps")
            .join("Microsoft.WinDbg_1.0.0.0_x64__8wekyb3d8bbwe")
            .join("amd64");
        let sdk_dbgeng = root
            .join("Windows Kits")
            .join("10")
            .join("Debuggers")
            .join(crate::runtime::debugger_arch());
        let system32 = root.join("Windows").join("System32");
        touch(app_dbgeng.join("dbgeng.dll"));
        touch(sdk_dbgeng.join("dbgeng.dll"));
        touch(system32.join("dbgeng.dll"));

        let detected = find_dbgeng_dir_from_roots(DbgEngSearchRoots {
            app_store_root: root.join("WindowsApps"),
            sdk_roots: vec![root.join("Windows Kits").join("10")],
            system_root: root.join("Windows"),
        })
        .expect("detect dbgeng");

        assert_eq!(detected, canonicalize(app_dbgeng));
    }

    #[test]
    fn dbgeng_install_detection_prefers_sdk_over_system32() {
        let root = unique_test_dir("dbgflow-install-dbgeng-sdk-order");
        let sdk_dbgeng = root
            .join("Windows Kits")
            .join("10")
            .join("Debuggers")
            .join(crate::runtime::debugger_arch());
        let system32 = root.join("Windows").join("System32");
        touch(sdk_dbgeng.join("dbgeng.dll"));
        touch(system32.join("dbgeng.dll"));

        let detected = find_dbgeng_dir_from_roots(DbgEngSearchRoots {
            app_store_root: root.join("WindowsApps"),
            sdk_roots: vec![root.join("Windows Kits").join("10")],
            system_root: root.join("Windows"),
        })
        .expect("detect dbgeng");

        assert_eq!(detected, canonicalize(sdk_dbgeng));
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        root
    }

    fn touch(path: PathBuf) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, b"dll").expect("write file");
    }

    fn canonicalize(path: PathBuf) -> PathBuf {
        std::fs::canonicalize(path).expect("canonicalize path")
    }
}
