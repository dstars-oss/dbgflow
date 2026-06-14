use dbgflow_common::process::{ChildIdentity, FallbackChildIdentity, ProcessLaunchConfig};
use dbgflow_common::proxy::ProxyEnvironment;
use dbgflow_debug::backend::dbgeng::DBGFLOW_DBGENG_DIR_ENV;
use dbgflow_reverse::ida::{
    DBGFLOW_IDA_DIR_ENV, DBGFLOW_IDA_PRO_MCP_SRC_ENV, DBGFLOW_IDA_PYTHON_ENV,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_BIND: &str = "127.0.0.1:7331";
pub const SERVICE_NAME: &str = "dbgflow-mcp";
pub const SERVICE_DISPLAY_NAME: &str = "dbgflow MCP Server";
pub const SERVICE_DESCRIPTION: &str = "dbgflow Streamable HTTP MCP server";
const CONFIG_VERSION: u32 = 1;
const KNOWN_PROXY_KEYS: &[&str] = &[
    "_NT_SYMBOL_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub proxy: ProxyEnvironment,
    pub process_launch: ProcessLaunchConfig,
    pub ttd_dir: Option<PathBuf>,
    pub dbgeng_dir: Option<PathBuf>,
    pub ida_install_dir: Option<PathBuf>,
    pub ida_python_executable: Option<PathBuf>,
    pub ida_vendor_src_dir: Option<PathBuf>,
    pub ida_max_workers: Option<usize>,
    pub symbol_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceSettings {
    pub name: String,
    pub display_name: String,
    pub install_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub config_path: PathBuf,
    pub service: ServiceSettings,
    pub app: AppConfig,
}

impl RuntimeConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let config_path = normalize_absolute_existing_file(path.as_ref(), "--config")?;
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|error| format!("read config {}: {error}", config_path.display()))?;
        let raw = toml::from_str::<RawRuntimeConfig>(&contents)
            .map_err(|error| format!("parse config {}: {error}", config_path.display()))?;
        Self::from_raw(config_path, raw)
    }

    fn from_raw(config_path: PathBuf, raw: RawRuntimeConfig) -> Result<Self, String> {
        if raw.version != CONFIG_VERSION {
            return Err(format!(
                "unsupported config version {}; expected {}",
                raw.version, CONFIG_VERSION
            ));
        }

        validate_service_name(&raw.service.name)?;
        if raw.service.display_name.trim().is_empty() {
            return Err("service.display_name must not be empty".to_string());
        }

        let install_root =
            normalize_absolute_required(&raw.service.install_root, "service.install_root")?;
        reject_dangerous_install_root(&install_root)?;
        let data_dir = normalize_absolute_required(&raw.server.data_dir, "server.data_dir")?;
        let bind = parse_bind(&raw.server.bind)?;
        if !path_starts_with_case_insensitive(&config_path, &install_root) {
            return Err(format!(
                "config path must be under service.install_root: {}",
                config_path.display()
            ));
        }
        if !path_starts_with_case_insensitive(&data_dir, &install_root) {
            return Err(format!(
                "server.data_dir must be under service.install_root: {}",
                data_dir.display()
            ));
        }

        let debugger = raw.debugger;
        let dbgeng_dir = debugger
            .as_ref()
            .and_then(|debugger| debugger.dbgeng_dir.as_ref())
            .map(|path| parse_dbgeng_dir_path(path, "debugger.dbgeng_dir"))
            .transpose()?;
        let symbol_path = debugger
            .and_then(|debugger| debugger.symbol_path)
            .map(|symbol_path| parse_symbol_path(&symbol_path, "debugger.symbol_path"))
            .transpose()?;
        let tools = raw.tools;
        let ttd_dir = tools
            .as_ref()
            .and_then(|tools| tools.ttd_dir.as_ref())
            .map(|path| parse_ttd_dir_path(path, "tools.ttd_dir"))
            .transpose()?
            .or_else(|| infer_ttd_dir_from_dbgeng_dir(dbgeng_dir.as_deref()));
        let reverse = raw.reverse;
        let ida_install_dir = reverse
            .as_ref()
            .and_then(|reverse| reverse.ida.as_ref())
            .and_then(|ida| ida.install_dir.as_ref())
            .map(|path| parse_ida_install_dir_path(path, "reverse.ida.install_dir"))
            .transpose()?;
        let ida_python_executable = reverse
            .as_ref()
            .and_then(|reverse| reverse.ida.as_ref())
            .and_then(|ida| ida.python_executable.as_ref())
            .map(|path| parse_existing_file_path(path, "reverse.ida.python_executable"))
            .transpose()?;
        let ida_vendor_src_dir = reverse
            .as_ref()
            .and_then(|reverse| reverse.ida.as_ref())
            .and_then(|ida| ida.vendor_src_dir.as_ref())
            .map(|path| parse_ida_vendor_src_dir(path, "reverse.ida.vendor_src_dir"))
            .transpose()?;
        let ida_max_workers = reverse
            .as_ref()
            .and_then(|reverse| reverse.ida.as_ref())
            .and_then(|ida| ida.max_workers)
            .map(parse_ida_max_workers)
            .transpose()?;
        let proxy = proxy_from_config(raw.proxy)?;
        let process_launch = process_launch_from_config(raw.process)?;

        Ok(Self {
            config_path,
            service: ServiceSettings {
                name: raw.service.name,
                display_name: raw.service.display_name,
                install_root,
            },
            app: AppConfig {
                bind,
                data_dir,
                proxy,
                process_launch,
                ttd_dir,
                dbgeng_dir,
                ida_install_dir,
                ida_python_executable,
                ida_vendor_src_dir,
                ida_max_workers,
                symbol_path,
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct ServiceProcessConfig {
    pub service_name: String,
    pub app: AppConfig,
}

#[derive(Debug, Clone)]
pub struct ServiceInstallConfig {
    pub config_path: PathBuf,
    pub service: ServiceSettings,
    pub app: AppConfig,
}

impl ServiceInstallConfig {
    pub fn bin_dir(&self) -> PathBuf {
        self.service.install_root.join("bin")
    }

    pub fn installed_exe(&self) -> PathBuf {
        self.bin_dir().join("dbgflow-mcp.exe")
    }

    pub fn normalized_command_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("service"),
            OsString::from("install"),
            OsString::from("--config"),
            self.config_path.as_os_str().to_os_string(),
        ]
    }

    pub fn service_launch_arguments(&self) -> Vec<OsString> {
        vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--config"),
            self.config_path.as_os_str().to_os_string(),
        ]
    }
}

#[derive(Debug, Clone)]
pub struct ServiceUninstallConfig {
    pub service_name: String,
    pub config_path: Option<PathBuf>,
}

impl ServiceUninstallConfig {
    pub fn normalized_command_args(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("service"),
            OsString::from("uninstall"),
            OsString::from("--service-name"),
            OsString::from(&self.service_name),
        ];
        if let Some(config_path) = &self.config_path {
            args.push(OsString::from("--config"));
            args.push(config_path.as_os_str().to_os_string());
        }
        args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledServiceCommand {
    pub executable_path: PathBuf,
    pub config_path: PathBuf,
}

pub fn parse_options<I>(args: I) -> Result<AppConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    if args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "--config" || arg.starts_with("--config="))
    }) {
        let config_path = parse_required_config_path(args, help_text())?;
        return Ok(RuntimeConfig::load(config_path)?.app);
    }
    parse_legacy_http_options(args)
}

fn parse_legacy_http_options(args: Vec<OsString>) -> Result<AppConfig, String> {
    let mut bind = parse_bind(DEFAULT_BIND)?;
    let mut data_dir = None;
    let mut proxy_url = None;
    let mut no_proxy = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;

        if let Some(value) = arg.strip_prefix("--bind=") {
            bind = parse_bind(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--data-dir=") {
            data_dir = Some(normalize_cli_path(Path::new(value), "--data-dir")?);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--proxy-url=") {
            proxy_url = Some(parse_non_empty(value, "--proxy-url")?);
            continue;
        }

        match arg.as_str() {
            "--bind" => {
                let value = next_value(&mut args, "--bind")?;
                bind = parse_bind(&value)?;
            }
            "--data-dir" => {
                let value = next_value(&mut args, "--data-dir")?;
                data_dir = Some(normalize_cli_path(Path::new(&value), "--data-dir")?);
            }
            "--proxy-url" => {
                let value = next_value(&mut args, "--proxy-url")?;
                proxy_url = Some(parse_non_empty(&value, "--proxy-url")?);
            }
            "--no-proxy" => no_proxy = true,
            "--help" | "-h" => return Err(help_text().to_string()),
            other => return Err(format!("unknown option: {other}\n\n{}", help_text())),
        }
    }

    let data_dir =
        data_dir.ok_or_else(|| format!("missing required --data-dir <path>\n\n{}", help_text()))?;
    let proxy = resolve_legacy_proxy(proxy_url, no_proxy)?;
    Ok(AppConfig {
        bind,
        data_dir,
        proxy,
        process_launch: ProcessLaunchConfig::default(),
        ttd_dir: None,
        dbgeng_dir: None,
        ida_install_dir: None,
        ida_python_executable: None,
        ida_vendor_src_dir: None,
        ida_max_workers: None,
        symbol_path: None,
    })
}

fn resolve_legacy_proxy(
    proxy_url: Option<String>,
    no_proxy: bool,
) -> Result<ProxyEnvironment, String> {
    if no_proxy && proxy_url.is_some() {
        return Err("--proxy-url and --no-proxy cannot be used together".to_string());
    }
    if no_proxy {
        return Ok(ProxyEnvironment::disabled());
    }
    if let Some(proxy_url) = proxy_url {
        return ProxyEnvironment::from_cli_proxy_url(&proxy_url).map_err(|error| error.to_string());
    }
    ProxyEnvironment::from_current_environment().map_err(|error| error.to_string())
}

fn parse_non_empty(value: &str, label: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    Ok(value.to_string())
}

fn normalize_cli_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("read current directory for {label}: {error}"))?
            .join(path)
    };
    normalize_path_lexically(&path)
}

pub fn parse_service_process_options<I>(args: I) -> Result<ServiceProcessConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let config_path = parse_required_config_path(args, help_text())?;
    let config = RuntimeConfig::load(config_path)?;
    Ok(ServiceProcessConfig {
        service_name: config.service.name,
        app: config.app,
    })
}

pub fn service_process_options_from_command_line<I>(args: I) -> Result<ServiceProcessConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _exe = args
        .next()
        .ok_or_else(|| "missing executable path".to_string())?;
    let command = next_command(&mut args, "top-level")?;
    if command != "service" {
        return Err(format!("expected service command, got: {command}"));
    }
    let subcommand = next_command(&mut args, "service")?;
    if subcommand != "run" {
        return Err(format!("expected service run, got service {subcommand}"));
    }
    parse_service_process_options(args)
}

pub fn parse_service_install_options<I>(args: I) -> Result<ServiceInstallConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let config_path = parse_required_config_path(args, service_install_help_text())?;
    let config = RuntimeConfig::load(config_path)?;
    Ok(ServiceInstallConfig {
        config_path: config.config_path,
        service: config.service,
        app: config.app,
    })
}

pub fn parse_service_uninstall_options<I>(args: I) -> Result<ServiceUninstallConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut service_name = None;
    let mut config_path = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;

        if let Some(value) = arg.strip_prefix("--service-name=") {
            service_name = Some(parse_service_name(value)?);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            config_path = Some(normalize_absolute_existing_file(
                Path::new(value),
                "--config",
            )?);
            continue;
        }

        match arg.as_str() {
            "--service-name" => {
                let value = next_value(&mut args, "--service-name")?;
                service_name = Some(parse_service_name(&value)?);
            }
            "--config" => {
                let value = next_value(&mut args, "--config")?;
                config_path = Some(normalize_absolute_existing_file(
                    Path::new(&value),
                    "--config",
                )?);
            }
            "--help" | "-h" => return Err(service_uninstall_help_text().to_string()),
            other => {
                return Err(format!(
                    "unknown option: {other}\n\n{}",
                    service_uninstall_help_text()
                ))
            }
        }
    }

    let service_name = match (service_name, config_path.as_ref()) {
        (Some(service_name), Some(config_path)) => {
            let config = RuntimeConfig::load(config_path)?;
            if config.service.name != service_name {
                return Err(format!(
                    "--service-name '{}' does not match config service.name '{}'",
                    service_name, config.service.name
                ));
            }
            service_name
        }
        (Some(service_name), None) => service_name,
        (None, Some(config_path)) => RuntimeConfig::load(config_path)?.service.name,
        (None, None) => SERVICE_NAME.to_string(),
    };

    Ok(ServiceUninstallConfig {
        service_name,
        config_path,
    })
}

pub fn help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp http --bind <addr> --data-dir <path> [options]\n  dbgflow-mcp http --config <path>                Run local HTTP MCP transport\n  dbgflow-mcp service run --config <path>         Run as a Windows service process\n  dbgflow-mcp service install --config <path>     Install and start the Windows service\n  dbgflow-mcp service uninstall [options]         Stop, uninstall, and remove install root\n  dbgflow-mcp worker session                      Run an internal debug session worker process\n\nHTTP options:\n  --bind <addr>                                   Loopback bind address; default 127.0.0.1:7331\n  --data-dir <path>                               Required for direct HTTP mode\n  --proxy-url <url>                               Sets _NT_SYMBOL_PROXY plus HTTP(S) proxy vars\n  --no-proxy                                     Clears known proxy vars for session workers"
}

pub fn service_install_help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp service install --config <path>\n\nOptions:\n  --config <path>                                  Required TOML config file"
}

pub fn service_uninstall_help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp service uninstall [options]\n\nOptions:\n  --service-name <name>                            Default: dbgflow-mcp\n  --config <path>                                  Fallback config path when the service is missing or has an unparsable command"
}

pub fn apply_runtime_environment(config: &AppConfig) {
    if let Some(dbgeng_dir) = &config.dbgeng_dir {
        std::env::set_var(DBGFLOW_DBGENG_DIR_ENV, dbgeng_dir);
    } else {
        std::env::remove_var(DBGFLOW_DBGENG_DIR_ENV);
    }
    if let Some(ida_install_dir) = &config.ida_install_dir {
        std::env::set_var(DBGFLOW_IDA_DIR_ENV, ida_install_dir);
    } else {
        std::env::remove_var(DBGFLOW_IDA_DIR_ENV);
    }
    if let Some(python_executable) = &config.ida_python_executable {
        std::env::set_var(DBGFLOW_IDA_PYTHON_ENV, python_executable);
    } else {
        std::env::remove_var(DBGFLOW_IDA_PYTHON_ENV);
    }
    if let Some(vendor_src_dir) = &config.ida_vendor_src_dir {
        std::env::set_var(DBGFLOW_IDA_PRO_MCP_SRC_ENV, vendor_src_dir);
    } else {
        std::env::remove_var(DBGFLOW_IDA_PRO_MCP_SRC_ENV);
    }
}

pub fn parse_installed_service_command(
    command_line: &str,
) -> Result<InstalledServiceCommand, String> {
    let args = split_windows_command_line(command_line)?;
    if args.len() < 4 {
        return Err("service command line is too short".to_string());
    }
    if args.get(1).map(String::as_str) != Some("service")
        || args.get(2).map(String::as_str) != Some("run")
    {
        return Err("service command line is not 'service run --config <path>'".to_string());
    }

    let mut config_path = None;
    let mut index = 3;
    while index < args.len() {
        if args[index] == "--config" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "service command line has --config without a value".to_string())?;
            config_path = Some(PathBuf::from(value));
            index += 2;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--config=") {
            config_path = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        index += 1;
    }

    let config_path = config_path
        .ok_or_else(|| "service command line does not include --config <path>".to_string())?;
    Ok(InstalledServiceCommand {
        executable_path: PathBuf::from(&args[0]),
        config_path,
    })
}

pub fn validate_install_root_removal(
    config: &RuntimeConfig,
    installed_exe: Option<&Path>,
) -> Result<PathBuf, String> {
    let install_root =
        normalize_absolute_required(&config.service.install_root, "service.install_root")?;
    reject_dangerous_install_root(&install_root)?;

    if !path_starts_with_case_insensitive(&config.config_path, &install_root) {
        return Err(format!(
            "refusing to remove install root because config is outside it: {}",
            config.config_path.display()
        ));
    }
    if !path_starts_with_case_insensitive(&config.app.data_dir, &install_root) {
        return Err(format!(
            "refusing to remove install root because data_dir is outside it: {}",
            config.app.data_dir.display()
        ));
    }
    if let Some(installed_exe) = installed_exe {
        let installed_exe =
            normalize_absolute_required(installed_exe, "installed service executable")?;
        if !path_starts_with_case_insensitive(&installed_exe, &install_root) {
            return Err(format!(
                "refusing to remove install root because service executable is outside it: {}",
                installed_exe.display()
            ));
        }
    }

    Ok(install_root)
}

#[derive(Debug, Deserialize)]
struct RawRuntimeConfig {
    version: u32,
    service: RawServiceConfig,
    server: RawServerConfig,
    debugger: Option<RawDebuggerConfig>,
    tools: Option<RawToolsConfig>,
    reverse: Option<RawReverseConfig>,
    process: Option<RawProcessConfig>,
    proxy: Option<RawProxyConfig>,
}

#[derive(Debug, Deserialize)]
struct RawServiceConfig {
    name: String,
    display_name: String,
    install_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RawServerConfig {
    bind: String,
    data_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RawDebuggerConfig {
    dbgeng_dir: Option<PathBuf>,
    symbol_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawToolsConfig {
    ttd_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RawReverseConfig {
    ida: Option<RawReverseIdaConfig>,
}

#[derive(Debug, Deserialize)]
struct RawReverseIdaConfig {
    install_dir: Option<PathBuf>,
    python_executable: Option<PathBuf>,
    vendor_src_dir: Option<PathBuf>,
    max_workers: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawProcessConfig {
    child_identity: Option<String>,
    fallback_child_identity: Option<String>,
    elevate_if_admin: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawProxyConfig {
    mode: String,
    url: Option<String>,
    env: Option<HashMap<String, String>>,
}

fn parse_required_config_path<I>(args: I, help: &str) -> Result<PathBuf, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut config_path = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;
        if let Some(value) = arg.strip_prefix("--config=") {
            config_path = Some(normalize_absolute_existing_file(
                Path::new(value),
                "--config",
            )?);
            continue;
        }
        match arg.as_str() {
            "--config" => {
                let value = next_value(&mut args, "--config")?;
                config_path = Some(normalize_absolute_existing_file(
                    Path::new(&value),
                    "--config",
                )?);
            }
            "--help" | "-h" => return Err(help.to_string()),
            other => return Err(format!("unknown option: {other}\n\n{help}")),
        }
    }
    config_path.ok_or_else(|| format!("missing required --config <path>\n\n{help}"))
}

fn next_value<I>(args: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .ok_or_else(|| format!("missing value for {option}"))?
        .into_string()
        .map_err(|_| format!("value for {option} must be valid UTF-8"))
}

fn next_command<I>(args: &mut I, label: &str) -> Result<String, String>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .ok_or_else(|| format!("missing {label} command"))?
        .into_string()
        .map_err(|_| format!("{label} command must be valid UTF-8"))
}

fn parse_service_name(value: &str) -> Result<String, String> {
    validate_service_name(value)?;
    Ok(value.to_string())
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

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    let bind = value
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid bind address {value}: {error}"))?;
    if !bind.ip().is_loopback() {
        return Err("bind address must be loopback".to_string());
    }
    Ok(bind)
}

fn parse_dbgeng_dir_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = parse_existing_dir_path(path, label)?;
    if !path.join("dbgeng.dll").is_file() {
        return Err(format!(
            "{label} must point to a directory containing dbgeng.dll: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn parse_ttd_dir_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = parse_existing_dir_path(path, label)?;
    if !path.join("TTD.exe").is_file() {
        return Err(format!(
            "{label} must point to a directory containing TTD.exe: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn parse_ida_install_dir_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = parse_existing_dir_path(path, label)?;
    dbgflow_reverse::ida::validate_ida_install_dir(&path)
        .map(|install| install.install_dir)
        .map_err(|error| error.to_string())
}

fn parse_ida_vendor_src_dir(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = parse_existing_dir_path(path, label)?;
    if !path.join("ida_pro_mcp").is_dir() {
        return Err(format!(
            "{label} must point to a vendored ida-pro-mcp src directory containing ida_pro_mcp: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn parse_ida_max_workers(max_workers: usize) -> Result<usize, String> {
    if max_workers == 0 {
        return Err("reverse.ida.max_workers must be greater than zero".to_string());
    }
    Ok(max_workers)
}

fn infer_ttd_dir_from_dbgeng_dir(dbgeng_dir: Option<&Path>) -> Option<PathBuf> {
    let dbgeng_dir = dbgeng_dir?;
    let ttd_dir = dbgeng_dir.join("ttd");
    ttd_dir.join("TTD.exe").is_file().then_some(ttd_dir)
}

fn parse_symbol_path(value: &str, label: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value
        .chars()
        .any(|ch| matches!(ch, '\0' | '\r' | '\n' | '\u{2028}' | '\u{2029}') || ch.is_control())
    {
        return Err(format!("{label} contains unsupported control characters"));
    }
    Ok(value.to_string())
}

fn parse_existing_dir_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = normalize_absolute_required(path, label)?;
    if !path.is_dir() {
        return Err(format!(
            "{label} must point to an existing directory: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn parse_existing_file_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = normalize_absolute_required(path, label)?;
    if !path.is_file() {
        return Err(format!(
            "{label} must point to an existing file: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn proxy_from_config(config: Option<RawProxyConfig>) -> Result<ProxyEnvironment, String> {
    let Some(config) = config else {
        return Ok(ProxyEnvironment::none());
    };
    match config.mode.as_str() {
        "none" => Ok(ProxyEnvironment::none()),
        "disabled" => Ok(ProxyEnvironment::disabled()),
        "url" => {
            let url = config
                .url
                .ok_or_else(|| "proxy.url is required when proxy.mode = \"url\"".to_string())?;
            ProxyEnvironment::from_cli_proxy_url(&url).map_err(|error| error.to_string())
        }
        "env" => {
            let env = config.env.unwrap_or_default();
            for (key, value) in &env {
                if !is_known_proxy_key(key) {
                    return Err(format!("proxy.env key is not supported: {key}"));
                }
                if value.is_empty() {
                    return Err(format!("proxy.env value must not be empty for key: {key}"));
                }
            }
            ProxyEnvironment::from_environment_map(&env).map_err(|error| error.to_string())
        }
        other => Err(format!(
            "proxy.mode must be one of none, disabled, url, env; got {other}"
        )),
    }
}

fn process_launch_from_config(
    config: Option<RawProcessConfig>,
) -> Result<ProcessLaunchConfig, String> {
    let mut process_launch = ProcessLaunchConfig::default();
    if let Some(config) = config {
        if let Some(child_identity) = config.child_identity {
            process_launch.child_identity =
                parse_child_identity(&child_identity, "process.child_identity")?;
        }
        if let Some(fallback_child_identity) = config.fallback_child_identity {
            process_launch.fallback_child_identity = parse_fallback_child_identity(
                &fallback_child_identity,
                "process.fallback_child_identity",
            )?;
        }
        if let Some(elevate_if_admin) = config.elevate_if_admin {
            process_launch.elevate_if_admin = elevate_if_admin;
        }
    }
    Ok(process_launch)
}

fn parse_child_identity(value: &str, label: &str) -> Result<ChildIdentity, String> {
    match value {
        "current_process" => Ok(ChildIdentity::CurrentProcess),
        "mcp_peer_session" => Ok(ChildIdentity::McpPeerSession),
        "active_interactive_session" => Ok(ChildIdentity::ActiveInteractiveSession),
        other => Err(format!(
            "{label} must be one of current_process, mcp_peer_session, active_interactive_session; got {other:?}"
        )),
    }
}

fn parse_fallback_child_identity(
    value: &str,
    label: &str,
) -> Result<FallbackChildIdentity, String> {
    match value {
        "current_process" => Ok(FallbackChildIdentity::CurrentProcess),
        "active_interactive_session" => Ok(FallbackChildIdentity::ActiveInteractiveSession),
        other => Err(format!(
            "{label} must be one of current_process, active_interactive_session; got {other:?}"
        )),
    }
}

fn is_known_proxy_key(key: &str) -> bool {
    KNOWN_PROXY_KEYS.contains(&key)
}

fn normalize_absolute_existing_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = normalize_absolute_required(path, label)?;
    if !path.is_file() {
        return Err(format!(
            "{label} must point to an existing file: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn normalize_absolute_required(path: &Path, label: &str) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if !path.is_absolute() {
        return Err(format!(
            "{label} must be an absolute path: {}",
            path.display()
        ));
    }
    normalize_path_lexically(path)
}

fn normalize_path_lexically(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("path escapes its root: {}", path.display()));
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn reject_dangerous_install_root(path: &Path) -> Result<(), String> {
    if path.components().count() <= 2 {
        return Err(format!(
            "service.install_root must not be a filesystem root: {}",
            path.display()
        ));
    }
    if path
        .file_name()
        .is_none_or(|name| !name.eq_ignore_ascii_case("dbgflow"))
    {
        return Err(format!(
            "service.install_root must be a dedicated 'dbgflow' directory: {}",
            path.display()
        ));
    }
    for (key, label) in [
        ("USERPROFILE", "the user profile root"),
        ("LOCALAPPDATA", "LOCALAPPDATA"),
        ("APPDATA", "APPDATA"),
        ("ProgramData", "ProgramData"),
        ("ProgramFiles", "ProgramFiles"),
        ("ProgramFiles(x86)", "ProgramFiles(x86)"),
    ] {
        if let Some(root) = std::env::var_os(key).map(PathBuf::from) {
            let root = normalize_path_lexically(&root)?;
            if path_eq_case_insensitive(path, &root) {
                return Err(format!(
                    "service.install_root must not be {label}: {}",
                    path.display()
                ));
            }
        }
    }
    for high_level in [Path::new(r"C:\Users"), Path::new(r"C:\ProgramData")] {
        if path_eq_case_insensitive(path, high_level) {
            return Err(format!(
                "service.install_root must not be a high-level system directory: {}",
                path.display()
            ));
        }
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        let user_profile = normalize_path_lexically(&user_profile)?;
        let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        if path_starts_with_case_insensitive(path, &user_profile)
            && local_app_data
                .as_ref()
                .is_none_or(|root| !path_starts_with_case_insensitive(path, root))
            && path.components().count() < user_profile.components().count() + 2
        {
            return Err(format!(
                "service.install_root must be a dedicated child directory under the user profile: {}",
                path.display()
            ));
        }
    }
    Ok(())
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

fn path_eq_case_insensitive(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn split_windows_command_line(value: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut arg = String::new();
    let mut in_quotes = false;
    let mut in_arg = false;
    let chars = value.chars().collect::<Vec<_>>();
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];
        if ch.is_whitespace() && !in_quotes {
            if in_arg {
                args.push(std::mem::take(&mut arg));
                in_arg = false;
            }
            index += 1;
            continue;
        }

        if ch == '\\' {
            let mut count = 0usize;
            while index < chars.len() && chars[index] == '\\' {
                count += 1;
                index += 1;
            }
            if index < chars.len() && chars[index] == '"' {
                arg.push_str(&"\\".repeat(count / 2));
                if count % 2 == 0 {
                    in_quotes = !in_quotes;
                } else {
                    arg.push('"');
                }
                in_arg = true;
                index += 1;
            } else {
                arg.push_str(&"\\".repeat(count));
                in_arg = true;
            }
            continue;
        }

        if ch == '"' {
            in_quotes = !in_quotes;
            in_arg = true;
            index += 1;
            continue;
        }

        arg.push(ch);
        in_arg = true;
        index += 1;
    }

    if in_quotes {
        return Err("service command line has an unterminated quote".to_string());
    }
    if in_arg {
        args.push(arg);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbgflow_common::proxy::ProxySource;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_runtime_config() {
        let root = unique_test_dir("runtime-config");
        let dbgeng = root.join("dbgeng");
        let ttd = root.join("ttd");
        let ida = root.join("ida");
        let python = root.join("python").join("python.exe");
        let vendor_src = root.join("vendor").join("ida-pro-mcp").join("src");
        std::fs::create_dir_all(&dbgeng).expect("create dbgeng dir");
        std::fs::create_dir_all(&ttd).expect("create ttd dir");
        touch(dbgeng.join("dbgeng.dll"));
        touch(ttd.join("TTD.exe"));
        touch(python.clone());
        std::fs::create_dir_all(vendor_src.join("ida_pro_mcp")).expect("create vendored package");
        for file_name in ["ida.exe", "ida.dll", "idalib.dll", "ida.hlp"] {
            touch(ida.join(file_name));
        }
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-dev"
display_name = "dbgflow Dev"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[debugger]
dbgeng_dir = "{}"
symbol_path = "srv*C:\\symbols*https://msdl.microsoft.com/download/symbols"

[tools]
ttd_dir = "{}"

[reverse.ida]
install_dir = "{}"
python_executable = "{}"
vendor_src_dir = "{}"
max_workers = 8

[proxy]
mode = "url"
url = "http://127.0.0.1:7897"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
                toml_path(&dbgeng),
                toml_path(&ttd),
                toml_path(&ida),
                toml_path(&python),
                toml_path(&vendor_src),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");
        assert_eq!(config.service.name, "dbgflow-dev");
        assert_eq!(config.service.display_name, "dbgflow Dev");
        assert_eq!(config.app.bind.to_string(), "127.0.0.1:7331");
        assert_eq!(config.app.proxy.source(), ProxySource::Cli);
        assert_eq!(
            config.app.proxy.value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("127.0.0.1:7897")
        );
        assert_eq!(
            config.app.symbol_path.as_deref(),
            Some("srv*C:\\symbols*https://msdl.microsoft.com/download/symbols")
        );
        assert_eq!(config.app.ttd_dir.as_deref(), Some(ttd.as_path()));
        assert_eq!(config.app.ida_install_dir.as_deref(), Some(ida.as_path()));
        assert_eq!(
            config.app.ida_python_executable.as_deref(),
            Some(python.as_path())
        );
        assert_eq!(
            config.app.ida_vendor_src_dir.as_deref(),
            Some(vendor_src.as_path())
        );
        assert_eq!(config.app.ida_max_workers, Some(8));
        assert_eq!(
            config.app.process_launch,
            ProcessLaunchConfig {
                child_identity: ChildIdentity::CurrentProcess,
                fallback_child_identity: FallbackChildIdentity::CurrentProcess,
                elevate_if_admin: false,
            }
        );
    }

    #[test]
    fn parses_process_launch_config() {
        let root = unique_test_dir("runtime-process");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[process]
child_identity = "mcp_peer_session"
fallback_child_identity = "active_interactive_session"
elevate_if_admin = true

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");

        assert_eq!(
            config.app.process_launch,
            ProcessLaunchConfig::installed_service_default()
        );
    }

    #[test]
    fn rejects_unknown_process_launch_identity() {
        let root = unique_test_dir("runtime-process-invalid");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[process]
child_identity = "browser_magic"

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let error = RuntimeConfig::load(&config_path).expect_err("reject invalid process config");

        assert!(error.contains("process.child_identity"));
        assert!(error.contains("browser_magic"));
    }

    #[test]
    fn derives_ttd_dir_from_dbgeng_dir_when_tools_ttd_dir_is_omitted() {
        let root = unique_test_dir("runtime-derived-ttd");
        let dbgeng = root.join("windbg").join("amd64");
        let derived_ttd = dbgeng.join("ttd");
        touch(dbgeng.join("dbgeng.dll"));
        touch(derived_ttd.join("TTD.exe"));
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[debugger]
dbgeng_dir = "{}"

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
                toml_path(&dbgeng),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");

        assert_eq!(config.app.ttd_dir.as_deref(), Some(derived_ttd.as_path()));
    }

    #[test]
    fn explicit_ttd_dir_overrides_dbgeng_derived_ttd_dir() {
        let root = unique_test_dir("runtime-explicit-ttd");
        let dbgeng = root.join("windbg").join("amd64");
        let explicit_ttd = root.join("explicit-ttd");
        touch(dbgeng.join("dbgeng.dll"));
        touch(dbgeng.join("ttd").join("TTD.exe"));
        touch(explicit_ttd.join("TTD.exe"));
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[debugger]
dbgeng_dir = "{}"

[tools]
ttd_dir = "{}"

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
                toml_path(&dbgeng),
                toml_path(&explicit_ttd),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");

        assert_eq!(config.app.ttd_dir.as_deref(), Some(explicit_ttd.as_path()));
    }

    #[test]
    fn does_not_derive_ttd_dir_when_dbgeng_ttd_directory_is_missing() {
        let root = unique_test_dir("runtime-no-derived-ttd");
        let dbgeng = root.join("windbg").join("amd64");
        touch(dbgeng.join("dbgeng.dll"));
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[debugger]
dbgeng_dir = "{}"

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
                toml_path(&dbgeng),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");

        assert!(config.app.ttd_dir.is_none());
    }

    #[test]
    fn parses_symbol_path_without_dbgeng_dir() {
        let root = unique_test_dir("runtime-symbol-path-only");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[debugger]
symbol_path = "cache*C:\\symbols;srv*https://msdl.microsoft.com/download/symbols"

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");

        assert!(config.app.dbgeng_dir.is_none());
        assert_eq!(
            config.app.symbol_path.as_deref(),
            Some("cache*C:\\symbols;srv*https://msdl.microsoft.com/download/symbols")
        );
    }

    #[test]
    fn rejects_symbol_path_control_characters() {
        let root = unique_test_dir("runtime-symbol-path-control");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[debugger]
symbol_path = "srv*C:\\symbols\r\n.shell dir"

[proxy]
mode = "none"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let error = RuntimeConfig::load(&config_path).expect_err("reject symbol path");

        assert!(error.contains("debugger.symbol_path contains unsupported control characters"));
    }

    #[test]
    fn rejects_config_outside_install_root() {
        let root = unique_test_dir("runtime-config-outside");
        let config_dir = unique_test_dir("runtime-config-outside-file");
        let config_path = config_dir.join("config.toml");
        write_minimal_config(&config_path, &root, &root.join("var"));

        let error = RuntimeConfig::load(&config_path).expect_err("reject outside config");
        assert!(error.contains("config path must be under service.install_root"));
    }

    #[test]
    fn rejects_install_root_that_is_not_dedicated_dbgflow_dir() {
        let base = unique_test_base_dir("runtime-config-dangerous-root");
        let install_root = base.join("not-dbgflow");
        std::fs::create_dir_all(&install_root).expect("create install root");
        let config_path = install_root.join("config.toml");
        write_minimal_config(&config_path, &install_root, &install_root.join("var"));

        let error = RuntimeConfig::load(&config_path).expect_err("reject dangerous root");
        assert!(error.contains("dedicated 'dbgflow' directory"));
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let root = unique_test_dir("runtime-config-bind");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-dev"
display_name = "dbgflow Dev"
install_root = "{}"

[server]
bind = "0.0.0.0:7331"
data_dir = "{}"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let error = RuntimeConfig::load(&config_path).expect_err("reject non-loopback");
        assert!(error.contains("bind address must be loopback"));
    }

    #[test]
    fn parses_http_options_from_config() {
        let root = unique_test_dir("runtime-http");
        let config_path = root.join("config.toml");
        write_minimal_config(&config_path, &root, &root.join("var"));

        let app = parse_options([OsString::from("--config"), config_path.into_os_string()])
            .expect("parse http options");

        assert_eq!(app.bind.to_string(), "127.0.0.1:7331");
        assert_eq!(app.proxy.source(), ProxySource::None);
    }

    #[test]
    fn parses_legacy_http_bind_and_data_dir_options() {
        let root = unique_test_dir("runtime-http-legacy");
        let data_dir = root.join("var");

        let app = parse_options([
            OsString::from("--bind"),
            OsString::from("127.0.0.1:0"),
            OsString::from("--data-dir"),
            data_dir.as_os_str().to_os_string(),
            OsString::from("--no-proxy"),
        ])
        .expect("parse legacy http options");

        assert_eq!(app.bind.to_string(), "127.0.0.1:0");
        assert_eq!(app.data_dir, data_dir);
        assert_eq!(app.proxy.source(), ProxySource::Disabled);
    }

    #[test]
    fn parses_legacy_http_relative_data_dir() {
        let app = parse_options([
            OsString::from("--data-dir"),
            OsString::from(".\\var"),
            OsString::from("--no-proxy"),
        ])
        .expect("parse relative data dir");

        assert!(app.data_dir.ends_with("var"));
        assert!(app.data_dir.is_absolute());
    }

    #[test]
    fn parses_legacy_http_proxy_url_option() {
        let root = unique_test_dir("runtime-http-proxy-url");
        let data_dir = root.join("var");

        let app = parse_options([
            OsString::from("--data-dir"),
            data_dir.as_os_str().to_os_string(),
            OsString::from("--proxy-url=http://127.0.0.1:7897"),
        ])
        .expect("parse proxy url");

        assert_eq!(app.proxy.source(), ProxySource::Cli);
        assert_eq!(
            app.proxy.value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("127.0.0.1:7897")
        );
    }

    #[test]
    fn rejects_legacy_http_without_data_dir() {
        let error = parse_options([OsString::from("--bind"), OsString::from("127.0.0.1:0")])
            .expect_err("reject missing data dir");

        assert!(error.contains("missing required --data-dir"));
    }

    #[test]
    fn rejects_conflicting_legacy_proxy_options() {
        let root = unique_test_dir("runtime-http-proxy-conflict");
        let error = parse_options([
            OsString::from("--data-dir"),
            root.join("var").as_os_str().to_os_string(),
            OsString::from("--proxy-url"),
            OsString::from("http://127.0.0.1:7897"),
            OsString::from("--no-proxy"),
        ])
        .expect_err("reject conflicting proxy options");

        assert!(error.contains("--proxy-url and --no-proxy"));
    }

    #[test]
    fn parses_service_install_options() {
        let root = unique_test_dir("runtime-install");
        let config_path = root.join("config.toml");
        write_minimal_config(&config_path, &root, &root.join("var"));

        let config = parse_service_install_options([
            OsString::from("--config"),
            config_path.as_os_str().to_os_string(),
        ])
        .expect("parse service install options");

        assert_eq!(
            config.normalized_command_args(),
            vec![
                OsString::from("service"),
                OsString::from("install"),
                OsString::from("--config"),
                config_path.as_os_str().to_os_string(),
            ]
        );
        assert_eq!(
            config.service_launch_arguments(),
            vec![
                OsString::from("service"),
                OsString::from("run"),
                OsString::from("--config"),
                config_path.as_os_str().to_os_string(),
            ]
        );
    }

    #[test]
    fn parses_service_uninstall_by_name() {
        let config = parse_service_uninstall_options([
            OsString::from("--service-name"),
            OsString::from("dbgflow-dev"),
        ])
        .expect("parse service uninstall");

        assert_eq!(config.service_name, "dbgflow-dev");
        assert!(config.config_path.is_none());
        assert_eq!(
            config.normalized_command_args(),
            vec![
                OsString::from("service"),
                OsString::from("uninstall"),
                OsString::from("--service-name"),
                OsString::from("dbgflow-dev"),
            ]
        );
    }

    #[test]
    fn parses_service_uninstall_name_from_config() {
        let root = unique_test_dir("runtime-uninstall-config");
        let config_path = root.join("config.toml");
        write_minimal_config(&config_path, &root, &root.join("var"));

        let config = parse_service_uninstall_options([
            OsString::from("--config"),
            config_path.as_os_str().to_os_string(),
        ])
        .expect("parse service uninstall");

        assert_eq!(config.service_name, "dbgflow-mcp");
        assert_eq!(config.config_path, Some(config_path));
    }

    #[test]
    fn parses_installed_service_command_line() {
        let parsed = parse_installed_service_command(
            r#""C:\Users\dstars\AppData\Local\dbgflow\bin\dbgflow-mcp.exe" service run --config "C:\Users\dstars\AppData\Local\dbgflow\config.toml""#,
        )
        .expect("parse service command line");

        assert_eq!(
            parsed.executable_path,
            PathBuf::from(r"C:\Users\dstars\AppData\Local\dbgflow\bin\dbgflow-mcp.exe")
        );
        assert_eq!(
            parsed.config_path,
            PathBuf::from(r"C:\Users\dstars\AppData\Local\dbgflow\config.toml")
        );
    }

    #[test]
    fn validate_removal_rejects_exe_outside_root() {
        let root = unique_test_dir("runtime-remove");
        let config_path = root.join("config.toml");
        write_minimal_config(&config_path, &root, &root.join("var"));
        let config = RuntimeConfig::load(&config_path).expect("load config");
        let outside = unique_test_dir("runtime-remove-outside").join("dbgflow-mcp.exe");

        let error =
            validate_install_root_removal(&config, Some(&outside)).expect_err("reject outside exe");

        assert!(error.contains("service executable is outside"));
    }

    #[test]
    fn proxy_env_derives_symbol_proxy_from_network_proxy() {
        let root = unique_test_dir("runtime-proxy-derived-symbol");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[proxy]
mode = "env"

[proxy.env]
http_proxy = "http://127.0.0.1:7897"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let config = RuntimeConfig::load(&config_path).expect("load config");

        assert_eq!(config.app.proxy.source(), ProxySource::Environment);
        assert_eq!(
            config.app.proxy.value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("127.0.0.1:7897")
        );
    }

    #[test]
    fn proxy_env_rejects_unknown_key() {
        let root = unique_test_dir("runtime-proxy-unknown");
        let config_path = root.join("config.toml");
        write_config(
            &config_path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[proxy]
mode = "env"

[proxy.env]
BAD_PROXY = "http://127.0.0.1:7897"
"#,
                toml_path(&root),
                toml_path(&root.join("var")),
            ),
        );

        let error = RuntimeConfig::load(&config_path).expect_err("reject proxy key");
        assert!(error.contains("proxy.env key is not supported"));
    }

    fn write_minimal_config(path: &Path, install_root: &Path, data_dir: &Path) {
        write_config(
            path,
            &format!(
                r#"
version = 1

[service]
name = "dbgflow-mcp"
display_name = "dbgflow MCP Server"
install_root = "{}"

[server]
bind = "127.0.0.1:7331"
data_dir = "{}"

[proxy]
mode = "none"
"#,
                toml_path(install_root),
                toml_path(data_dir),
            ),
        );
    }

    fn write_config(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create config parent");
        }
        std::fs::write(path, contents.trim_start()).expect("write config");
    }

    fn touch(path: PathBuf) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, b"").expect("touch file");
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let path = unique_test_base_dir(name).join("dbgflow");
        std::fs::create_dir_all(&path).expect("create unique dir");
        path
    }

    fn unique_test_base_dir(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{name}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create unique dir");
        path
    }

    fn toml_path(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }
}
