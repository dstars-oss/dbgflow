use dbgflow_core::proxy::ProxyEnvironment;
use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

pub const DEFAULT_BIND: &str = "127.0.0.1:7331";
pub const SERVICE_NAME: &str = "dbgflow-mcp";
pub const SERVICE_DISPLAY_NAME: &str = "dbgflow MCP Server";
pub const SERVICE_DESCRIPTION: &str = "dbgflow Streamable HTTP MCP server";
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
    pub sysinternals_dir: Option<PathBuf>,
}

impl AppConfig {
    pub fn app_proxy(&self) -> &ProxyEnvironment {
        &self.proxy
    }
}

#[derive(Debug, Clone)]
pub struct ServiceProcessConfig {
    pub service_name: String,
    pub app: AppConfig,
}

#[derive(Debug, Clone)]
pub struct ServiceInstallConfig {
    pub service_name: String,
    pub display_name: String,
    pub bind: SocketAddr,
    pub install_root: PathBuf,
    pub sysinternals_dir: Option<PathBuf>,
}

impl ServiceInstallConfig {
    pub fn data_dir(&self) -> PathBuf {
        self.install_root.join("var")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.install_root.join("bin")
    }

    pub fn installed_exe(&self) -> PathBuf {
        self.bin_dir().join("dbgflow-mcp.exe")
    }

    pub fn normalized_command_args(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("service"),
            OsString::from("install"),
            OsString::from("--service-name"),
            OsString::from(&self.service_name),
            OsString::from("--display-name"),
            OsString::from(&self.display_name),
            OsString::from("--bind"),
            OsString::from(self.bind.to_string()),
            OsString::from("--install-root"),
            self.install_root.as_os_str().to_os_string(),
        ];
        if let Some(sysinternals_dir) = &self.sysinternals_dir {
            args.push(OsString::from("--sysinternals-dir"));
            args.push(sysinternals_dir.as_os_str().to_os_string());
        }
        args
    }

    pub fn service_launch_arguments(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--service-name"),
            OsString::from(&self.service_name),
            OsString::from("--bind"),
            OsString::from(self.bind.to_string()),
            OsString::from("--data-dir"),
            self.data_dir().as_os_str().to_os_string(),
        ];
        if let Some(sysinternals_dir) = &self.sysinternals_dir {
            args.push(OsString::from("--sysinternals-dir"));
            args.push(sysinternals_dir.as_os_str().to_os_string());
        }
        args
    }
}

#[derive(Debug, Clone)]
pub struct ServiceUninstallConfig {
    pub service_name: String,
    pub install_root: PathBuf,
    pub remove_install_files: bool,
}

impl ServiceUninstallConfig {
    pub fn normalized_command_args(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("service"),
            OsString::from("uninstall"),
            OsString::from("--service-name"),
            OsString::from(&self.service_name),
            OsString::from("--install-root"),
            self.install_root.as_os_str().to_os_string(),
        ];
        if self.remove_install_files {
            args.push(OsString::from("--remove-install-files"));
        }
        args
    }
}

pub fn parse_options<I>(args: I) -> Result<AppConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let env = std::env::vars().collect::<HashMap<_, _>>();
    parse_options_with_env(args, &env)
}

fn parse_options_with_env<I>(args: I, env: &HashMap<String, String>) -> Result<AppConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut bind = DEFAULT_BIND.parse().expect("valid default bind address");
    let mut data_dir = None;
    let mut proxy_url = None;
    let mut no_proxy = false;
    let mut sysinternals_dir = None;
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
            data_dir = Some(PathBuf::from(value));
            continue;
        }
        if let Some(value) = arg.strip_prefix("--proxy-url=") {
            proxy_url = Some(parse_non_empty(value, "--proxy-url")?);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--sysinternals-dir=") {
            sysinternals_dir = Some(parse_existing_dir(value, "--sysinternals-dir")?);
            continue;
        }

        match arg.as_str() {
            "--bind" => {
                let value = next_value(&mut args, "--bind")?;
                bind = parse_bind(&value)?;
            }
            "--data-dir" => {
                let value = next_value(&mut args, "--data-dir")?;
                data_dir = Some(PathBuf::from(value));
            }
            "--proxy-url" => {
                let value = next_value(&mut args, "--proxy-url")?;
                proxy_url = Some(parse_non_empty(&value, "--proxy-url")?);
            }
            "--sysinternals-dir" => {
                let value = next_value(&mut args, "--sysinternals-dir")?;
                sysinternals_dir = Some(parse_existing_dir(&value, "--sysinternals-dir")?);
            }
            "--no-proxy" => no_proxy = true,
            "--help" | "-h" => return Err(help_text().to_string()),
            other => return Err(format!("unknown option: {other}\n\n{}", help_text())),
        }
    }

    let data_dir =
        data_dir.ok_or_else(|| format!("missing required --data-dir <path>\n\n{}", help_text()))?;
    let proxy = resolve_proxy(proxy_url, no_proxy, env)?;
    Ok(AppConfig {
        bind,
        data_dir,
        proxy,
        sysinternals_dir,
    })
}

pub fn parse_service_process_options<I>(args: I) -> Result<ServiceProcessConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let env = std::env::vars().collect::<HashMap<_, _>>();
    parse_service_process_options_with_env(args, &env)
}

fn parse_service_process_options_with_env<I>(
    args: I,
    env: &HashMap<String, String>,
) -> Result<ServiceProcessConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut service_name = SERVICE_NAME.to_string();
    let mut bind = DEFAULT_BIND.parse().expect("valid default bind address");
    let mut data_dir = None;
    let mut proxy_url = None;
    let mut no_proxy = false;
    let mut sysinternals_dir = None;
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
            data_dir = Some(PathBuf::from(value));
            continue;
        }
        if let Some(value) = arg.strip_prefix("--service-name=") {
            service_name = parse_service_name(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--proxy-url=") {
            proxy_url = Some(parse_non_empty(value, "--proxy-url")?);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--sysinternals-dir=") {
            sysinternals_dir = Some(parse_existing_dir(value, "--sysinternals-dir")?);
            continue;
        }

        match arg.as_str() {
            "--bind" => {
                let value = next_value(&mut args, "--bind")?;
                bind = parse_bind(&value)?;
            }
            "--data-dir" => {
                let value = next_value(&mut args, "--data-dir")?;
                data_dir = Some(PathBuf::from(value));
            }
            "--service-name" => {
                let value = next_value(&mut args, "--service-name")?;
                service_name = parse_service_name(&value)?;
            }
            "--proxy-url" => {
                let value = next_value(&mut args, "--proxy-url")?;
                proxy_url = Some(parse_non_empty(&value, "--proxy-url")?);
            }
            "--sysinternals-dir" => {
                let value = next_value(&mut args, "--sysinternals-dir")?;
                sysinternals_dir = Some(parse_existing_dir(&value, "--sysinternals-dir")?);
            }
            "--no-proxy" => no_proxy = true,
            "--help" | "-h" => return Err(help_text().to_string()),
            other => return Err(format!("unknown option: {other}\n\n{}", help_text())),
        }
    }

    let data_dir =
        data_dir.ok_or_else(|| format!("missing required --data-dir <path>\n\n{}", help_text()))?;
    let proxy = resolve_proxy(proxy_url, no_proxy, env)?;
    Ok(ServiceProcessConfig {
        service_name,
        app: AppConfig {
            bind,
            data_dir,
            proxy,
            sysinternals_dir,
        },
    })
}

pub fn service_process_options_from_command_line<I>(args: I) -> Result<ServiceProcessConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let env = std::env::vars().collect::<HashMap<_, _>>();
    service_process_options_from_command_line_with_env(args, &env)
}

fn service_process_options_from_command_line_with_env<I>(
    args: I,
    env: &HashMap<String, String>,
) -> Result<ServiceProcessConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _exe = args
        .next()
        .ok_or_else(|| "missing executable path".to_string())?;
    let command = next_command(&mut args, "service")?;
    if command != "service" {
        return Err(format!("expected service command, got {command}"));
    }
    let service_command = next_command(&mut args, "service run")?;
    if service_command != "run" {
        return Err(format!(
            "expected service run command, got service {service_command}"
        ));
    }
    parse_service_process_options_with_env(args, env)
}

pub fn parse_service_install_options<I>(args: I) -> Result<ServiceInstallConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut service_name = SERVICE_NAME.to_string();
    let mut display_name = SERVICE_DISPLAY_NAME.to_string();
    let mut bind = DEFAULT_BIND.parse().expect("valid default bind address");
    let mut install_root = None;
    let mut sysinternals_dir = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;

        if let Some(value) = arg.strip_prefix("--service-name=") {
            service_name = parse_service_name(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--display-name=") {
            display_name = parse_non_empty(value, "--display-name")?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--bind=") {
            bind = parse_bind(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--install-root=") {
            install_root = Some(PathBuf::from(value));
            continue;
        }
        if let Some(value) = arg.strip_prefix("--sysinternals-dir=") {
            sysinternals_dir = Some(parse_existing_dir(value, "--sysinternals-dir")?);
            continue;
        }
        match arg.as_str() {
            "--service-name" => {
                let value = next_value(&mut args, "--service-name")?;
                service_name = parse_service_name(&value)?;
            }
            "--display-name" => {
                let value = next_value(&mut args, "--display-name")?;
                display_name = parse_non_empty(&value, "--display-name")?;
            }
            "--bind" => {
                let value = next_value(&mut args, "--bind")?;
                bind = parse_bind(&value)?;
            }
            "--install-root" => {
                let value = next_value(&mut args, "--install-root")?;
                install_root = Some(PathBuf::from(value));
            }
            "--sysinternals-dir" => {
                let value = next_value(&mut args, "--sysinternals-dir")?;
                sysinternals_dir = Some(parse_existing_dir(&value, "--sysinternals-dir")?);
            }
            "--help" | "-h" => return Err(service_install_help_text().to_string()),
            other => {
                return Err(format!(
                    "unknown option: {other}\n\n{}",
                    service_install_help_text()
                ))
            }
        }
    }

    Ok(ServiceInstallConfig {
        service_name,
        display_name,
        bind,
        install_root: match install_root {
            Some(path) => path,
            None => default_install_root()?,
        },
        sysinternals_dir,
    })
}

pub fn parse_service_uninstall_options<I>(args: I) -> Result<ServiceUninstallConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut service_name = SERVICE_NAME.to_string();
    let mut install_root = None;
    let mut remove_install_files = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;

        if let Some(value) = arg.strip_prefix("--service-name=") {
            service_name = parse_service_name(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--install-root=") {
            install_root = Some(PathBuf::from(value));
            continue;
        }

        match arg.as_str() {
            "--service-name" => {
                let value = next_value(&mut args, "--service-name")?;
                service_name = parse_service_name(&value)?;
            }
            "--install-root" => {
                let value = next_value(&mut args, "--install-root")?;
                install_root = Some(PathBuf::from(value));
            }
            "--remove-install-files" => remove_install_files = true,
            "--help" | "-h" => return Err(service_uninstall_help_text().to_string()),
            other => {
                return Err(format!(
                    "unknown option: {other}\n\n{}",
                    service_uninstall_help_text()
                ))
            }
        }
    }

    Ok(ServiceUninstallConfig {
        service_name,
        install_root: match install_root {
            Some(path) => path,
            None => default_install_root()?,
        },
        remove_install_files,
    })
}

pub fn help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp http --data-dir <path> [options]        Run local HTTP MCP transport\n  dbgflow-mcp service run --data-dir <path> [options] Run as a Windows service process\n  dbgflow-mcp service install [options]               Install and start the Windows service\n  dbgflow-mcp service uninstall [options]             Stop and uninstall the Windows service\n  dbgflow-mcp worker session                          Run an internal session worker process\n\nRuntime options:\n  --bind <addr:port>                                  Default: 127.0.0.1:7331\n  --data-dir <path>                                   Required. Uses <path>\\artifacts and <path>\\logs\n  --service-name <name>                               Service process name. Default: dbgflow-mcp\n  --proxy-url <url>                                   Sets _NT_SYMBOL_PROXY plus HTTP(S) proxy vars for all session workers\n  --no-proxy                                         Clears known proxy vars for all session workers\n  --sysinternals-dir <path>                           Optional Sysinternals directory for Procmon-based features\n  Default proxy behavior inherits non-empty known proxy environment variables when no proxy option is passed"
}

pub fn service_install_help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp service install [options]\n\nOptions:\n  --service-name <name>                               Default: dbgflow-mcp\n  --display-name <name>                               Default: dbgflow MCP Server\n  --bind <addr:port>                                  Default: 127.0.0.1:7331\n  --install-root <path>                               Default: %LOCALAPPDATA%\\dbgflow\n  --sysinternals-dir <path>                           Optional Sysinternals directory for Procmon-based features"
}

pub fn service_uninstall_help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp service uninstall [options]\n\nOptions:\n  --service-name <name>                               Default: dbgflow-mcp\n  --install-root <path>                               Default: %LOCALAPPDATA%\\dbgflow\n  --remove-install-files                              Remove <install-root>\\bin; artifacts and logs stay under <install-root>\\var"
}

pub fn remove_install_files_target(install_root: PathBuf) -> Result<PathBuf, String> {
    let root = normalize_absolute_path(&install_root)?;
    let bin_dir = normalize_absolute_path(&install_root.join("bin"))?;
    if !path_starts_with(&bin_dir, &root) {
        return Err(format!(
            "refusing to remove path outside install root: {}",
            bin_dir.display()
        ));
    }
    Ok(bin_dir)
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

fn parse_non_empty(value: &str, option: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err(format!("{option} must not be empty"));
    }
    Ok(value.to_string())
}

fn parse_existing_dir(value: &str, option: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(parse_non_empty(value, option)?);
    if !path.is_dir() {
        return Err(format!(
            "{option} must point to an existing directory: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn parse_service_name(value: &str) -> Result<String, String> {
    let service_name = parse_non_empty(value, "--service-name")?;
    if service_name
        .chars()
        .any(|ch| matches!(ch, '/' | '\\' | '*' | '?' | '[' | ']') || ch.is_control())
    {
        return Err(
            "--service-name must not contain path separators, wildcards, or control characters"
                .to_string(),
        );
    }
    Ok(service_name)
}

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    let bind: SocketAddr = value
        .parse()
        .map_err(|error| format!("invalid bind address {value}: {error}"))?;
    if !bind.ip().is_loopback() {
        return Err(format!(
            "bind address must be loopback; dbgflow does not support remote HTTP access: {value}"
        ));
    }
    Ok(bind)
}

fn resolve_proxy(
    proxy_url: Option<String>,
    no_proxy: bool,
    env: &HashMap<String, String>,
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
    let env = proxy_environment_map_without_empty_known_keys(env);
    ProxyEnvironment::from_environment_map(&env).map_err(|error| error.to_string())
}

fn proxy_environment_map_without_empty_known_keys(
    env: &HashMap<String, String>,
) -> HashMap<String, String> {
    env.iter()
        .filter(|(key, value)| !(value.is_empty() && is_known_proxy_key(key)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn is_known_proxy_key(key: &str) -> bool {
    KNOWN_PROXY_KEYS.contains(&key)
}

fn default_install_root() -> Result<PathBuf, String> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| "LOCALAPPDATA is not set; pass --install-root <path>".to_string())?;
    Ok(PathBuf::from(local_app_data).join("dbgflow"))
}

fn normalize_absolute_path(path: &std::path::Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve current directory: {error}"))?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn path_starts_with(path: &std::path::Path, base: &std::path::Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{
        parse_options_with_env, parse_service_install_options,
        parse_service_process_options_with_env, parse_service_uninstall_options,
        remove_install_files_target, service_process_options_from_command_line_with_env,
        DEFAULT_BIND, SERVICE_NAME,
    };
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn env(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn parses_runtime_options() {
        let config = parse_options_with_env(
            [
                OsString::from("--bind"),
                OsString::from("127.0.0.1:9000"),
                OsString::from("--data-dir=C:\\dbgflow\\var"),
            ],
            &env(&[("HTTP_PROXY", "")]),
        )
        .expect("parse options");

        assert_eq!(config.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(config.data_dir, PathBuf::from("C:\\dbgflow\\var"));
    }

    #[test]
    fn parses_sysinternals_dir_for_http_runtime() {
        let root =
            std::env::temp_dir().join(format!("dbgflow-sysinternals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create sysinternals dir");

        let config = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--sysinternals-dir"),
                root.as_os_str().to_os_string(),
            ],
            &env(&[]),
        )
        .expect("parse options");

        assert_eq!(config.sysinternals_dir.as_deref(), Some(root.as_path()));
    }

    #[test]
    fn uses_default_bind_with_required_data_dir() {
        let config = parse_options_with_env(
            [OsString::from("--data-dir"), OsString::from(".\\var")],
            &env(&[("HTTP_PROXY", "")]),
        )
        .expect("parse options");
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(config.data_dir, PathBuf::from(".\\var"));
    }

    #[test]
    fn parses_http_proxy_url_option() {
        let config = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--proxy-url"),
                OsString::from("http://127.0.0.1:7897"),
            ],
            &env(&[]),
        )
        .expect("parse options");

        assert_eq!(
            config.app_proxy().value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("127.0.0.1:7897")
        );
        assert_eq!(
            config.app_proxy().value_for("HTTP_PROXY").as_deref(),
            Some("http://127.0.0.1:7897")
        );
    }

    #[test]
    fn parses_no_proxy_option() {
        let config = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--no-proxy"),
            ],
            &env(&[]),
        )
        .expect("parse options");

        assert_eq!(
            config.app_proxy().source(),
            dbgflow_core::proxy::ProxySource::Disabled
        );
    }

    #[test]
    fn rejects_conflicting_proxy_options() {
        let error = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--proxy-url"),
                OsString::from("http://127.0.0.1:7897"),
                OsString::from("--no-proxy"),
            ],
            &env(&[]),
        )
        .expect_err("reject conflicting proxy options");

        assert!(error.contains("cannot be used together"));
    }

    #[test]
    fn rejects_conflicting_proxy_options_in_either_order() {
        let no_proxy_first = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--no-proxy"),
                OsString::from("--proxy-url=http://127.0.0.1:7897"),
            ],
            &env(&[]),
        )
        .expect_err("reject conflicting proxy options");
        let proxy_url_first = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--proxy-url=http://127.0.0.1:7897"),
                OsString::from("--no-proxy"),
            ],
            &env(&[]),
        )
        .expect_err("reject conflicting proxy options");

        assert!(no_proxy_first.contains("cannot be used together"));
        assert!(proxy_url_first.contains("cannot be used together"));
    }

    #[test]
    fn inherits_proxy_from_non_empty_environment() {
        let config = parse_options_with_env(
            [OsString::from("--data-dir"), OsString::from(".\\var")],
            &env(&[
                ("_NT_SYMBOL_PROXY", "symproxy:80"),
                ("HTTP_PROXY", "http://proxy:8080"),
            ]),
        )
        .expect("parse options");

        assert_eq!(
            config.app_proxy().source(),
            dbgflow_core::proxy::ProxySource::Environment
        );
        assert_eq!(
            config.app_proxy().value_for("_NT_SYMBOL_PROXY").as_deref(),
            Some("symproxy:80")
        );
        assert_eq!(
            config.app_proxy().value_for("HTTP_PROXY").as_deref(),
            Some("http://proxy:8080")
        );
    }

    #[test]
    fn no_proxy_wins_over_environment_proxy_entries() {
        let config = parse_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from(".\\var"),
                OsString::from("--no-proxy"),
            ],
            &env(&[
                ("_NT_SYMBOL_PROXY", "symproxy:80"),
                ("HTTP_PROXY", "http://proxy:8080"),
            ]),
        )
        .expect("parse options");

        assert_eq!(
            config.app_proxy().source(),
            dbgflow_core::proxy::ProxySource::Disabled
        );
    }

    #[test]
    fn rejects_missing_data_dir() {
        let error = parse_options_with_env([], &env(&[])).expect_err("reject missing data dir");
        assert!(error.contains("--data-dir"));
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let error = parse_options_with_env(
            [OsString::from("--bind"), OsString::from("0.0.0.0:7331")],
            &env(&[]),
        )
        .expect_err("reject non-loopback bind");

        assert!(error.contains("loopback"));
    }

    #[test]
    fn rejects_removed_directory_options() {
        let artifact_error = parse_options_with_env(
            [OsString::from("--artifact-root=C:\\dbgflow\\artifacts")],
            &env(&[]),
        )
        .expect_err("reject artifact-root");
        assert!(artifact_error.contains("unknown option"));

        let log_error = parse_options_with_env(
            [OsString::from("--log-dir"), OsString::from("C:\\logs")],
            &env(&[]),
        )
        .expect_err("reject log-dir");
        assert!(log_error.contains("unknown option"));
    }

    #[test]
    fn parses_service_process_options_with_service_name() {
        let config = parse_service_process_options_with_env(
            [
                OsString::from("--service-name"),
                OsString::from("dbgflow-dev"),
                OsString::from("--bind"),
                OsString::from("127.0.0.1:9001"),
                OsString::from("--data-dir"),
                OsString::from("C:\\dbgflow\\var"),
            ],
            &env(&[("HTTP_PROXY", "")]),
        )
        .expect("parse service process options");

        assert_eq!(config.service_name, "dbgflow-dev");
        assert_eq!(config.app.bind.to_string(), "127.0.0.1:9001");
        assert_eq!(config.app.data_dir, PathBuf::from("C:\\dbgflow\\var"));
    }

    #[test]
    fn parses_service_process_proxy_url_option() {
        let config = parse_service_process_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from("C:\\dbgflow\\var"),
                OsString::from("--proxy-url=http://127.0.0.1:7897"),
            ],
            &env(&[]),
        )
        .expect("parse service process options");

        assert_eq!(
            config
                .app
                .app_proxy()
                .value_for("_NT_SYMBOL_PROXY")
                .as_deref(),
            Some("127.0.0.1:7897")
        );
    }

    #[test]
    fn parses_service_run_process_options_from_full_command_line() {
        let config = service_process_options_from_command_line_with_env(
            [
                OsString::from("dbgflow-mcp.exe"),
                OsString::from("service"),
                OsString::from("run"),
                OsString::from("--service-name"),
                OsString::from("dbgflow-dev"),
                OsString::from("--bind"),
                OsString::from("127.0.0.1:9001"),
                OsString::from("--data-dir"),
                OsString::from("C:\\dbgflow\\var"),
            ],
            &env(&[("HTTP_PROXY", "")]),
        )
        .expect("parse service run command line");

        assert_eq!(config.service_name, "dbgflow-dev");
        assert_eq!(config.app.bind.to_string(), "127.0.0.1:9001");
        assert_eq!(config.app.data_dir, PathBuf::from("C:\\dbgflow\\var"));
    }

    #[test]
    fn rejects_legacy_service_process_command_line_without_run() {
        let error = service_process_options_from_command_line_with_env(
            [
                OsString::from("dbgflow-mcp.exe"),
                OsString::from("service"),
                OsString::from("--data-dir"),
                OsString::from("C:\\dbgflow\\var"),
            ],
            &env(&[]),
        )
        .expect_err("reject legacy service process command line");

        assert!(error.contains("expected service run"));
    }

    #[test]
    fn parses_service_install_options() {
        let config = parse_service_install_options([
            OsString::from("--service-name"),
            OsString::from("dbgflow-dev"),
            OsString::from("--display-name"),
            OsString::from("dbgflow Dev"),
            OsString::from("--bind=127.0.0.1:9002"),
            OsString::from("--install-root"),
            OsString::from("C:\\dbgflow"),
        ])
        .expect("parse service install options");

        assert_eq!(config.service_name, "dbgflow-dev");
        assert_eq!(config.display_name, "dbgflow Dev");
        assert_eq!(config.bind.to_string(), "127.0.0.1:9002");
        assert_eq!(config.install_root, PathBuf::from("C:\\dbgflow"));
        assert!(!config
            .normalized_command_args()
            .iter()
            .any(|arg| arg == "--repo-root"));
    }

    #[test]
    fn service_install_launch_arguments_use_service_run_subcommand() {
        let config = parse_service_install_options([
            OsString::from("--service-name"),
            OsString::from("dbgflow-dev"),
            OsString::from("--install-root=C:\\dbgflow"),
        ])
        .expect("parse service install options");

        assert_eq!(
            config.service_launch_arguments(),
            vec![
                OsString::from("service"),
                OsString::from("run"),
                OsString::from("--service-name"),
                OsString::from("dbgflow-dev"),
                OsString::from("--bind"),
                OsString::from("127.0.0.1:7331"),
                OsString::from("--data-dir"),
                OsString::from("C:\\dbgflow\\var"),
            ]
        );
    }

    #[test]
    fn service_install_launch_arguments_include_sysinternals_dir_when_configured() {
        let root = std::env::temp_dir().join(format!(
            "dbgflow-install-sysinternals-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create sysinternals dir");

        let config = parse_service_install_options([
            OsString::from("--install-root=C:\\dbgflow"),
            OsString::from("--sysinternals-dir"),
            root.as_os_str().to_os_string(),
        ])
        .expect("parse service install options");

        assert!(config
            .service_launch_arguments()
            .windows(2)
            .any(|pair| pair[0] == "--sysinternals-dir"
                && pair[1] == root.as_os_str().to_os_string()));
    }

    #[test]
    fn service_install_launch_arguments_do_not_include_proxy_url() {
        let config = parse_service_install_options([
            OsString::from("--service-name"),
            OsString::from("dbgflow-dev"),
            OsString::from("--install-root=C:\\dbgflow"),
        ])
        .expect("parse service install options");

        for arg in config
            .service_launch_arguments()
            .iter()
            .chain(config.normalized_command_args().iter())
        {
            let arg = arg.to_string_lossy();
            assert!(!arg.contains("7897"));
            assert_ne!(arg, "--proxy-url");
            assert!(!arg.starts_with("--proxy-url="));
            assert_ne!(arg, "--no-proxy");
        }
    }

    #[test]
    fn service_install_rejects_invalid_service_names() {
        for service_name in [
            "bad\\name",
            "bad/name",
            "bad*name",
            "bad?name",
            "bad[name]",
            "bad\u{81}name",
        ] {
            let error = parse_service_install_options([
                OsString::from("--service-name"),
                OsString::from(service_name),
                OsString::from("--install-root=C:\\dbgflow"),
            ])
            .expect_err("reject invalid service name");

            assert!(
                error.contains("--service-name"),
                "unexpected error for {service_name:?}: {error}"
            );
        }
    }

    #[test]
    fn service_process_rejects_invalid_service_name() {
        let error = parse_service_process_options_with_env(
            [
                OsString::from("--service-name"),
                OsString::from("bad\\name"),
                OsString::from("--data-dir=C:\\dbgflow\\var"),
            ],
            &env(&[]),
        )
        .expect_err("reject invalid service name");

        assert!(error.contains("--service-name"));
    }

    #[test]
    fn service_uninstall_rejects_invalid_service_name() {
        let error = parse_service_uninstall_options([
            OsString::from("--service-name"),
            OsString::from("bad*name"),
            OsString::from("--install-root=C:\\dbgflow"),
        ])
        .expect_err("reject invalid service name");

        assert!(error.contains("--service-name"));
    }

    #[test]
    fn service_install_rejects_removed_repo_root_option() {
        let error = parse_service_install_options([
            OsString::from("--install-root=C:\\dbgflow"),
            OsString::from("--repo-root=D:\\Repos\\Project\\dbgflow"),
        ])
        .expect_err("reject repo-root");

        assert!(error.contains("unknown option"));
    }

    #[test]
    fn service_install_rejects_non_loopback_bind() {
        let error = parse_service_install_options([
            OsString::from("--bind"),
            OsString::from("0.0.0.0:7331"),
            OsString::from("--install-root"),
            OsString::from("C:\\dbgflow"),
        ])
        .expect_err("reject non-loopback bind");

        assert!(error.contains("loopback"));
    }

    #[test]
    fn parses_service_uninstall_options() {
        let config = parse_service_uninstall_options([
            OsString::from("--service-name"),
            OsString::from("dbgflow-dev"),
            OsString::from("--install-root=C:\\dbgflow"),
            OsString::from("--remove-install-files"),
        ])
        .expect("parse service uninstall options");

        assert_eq!(config.service_name, "dbgflow-dev");
        assert_eq!(config.install_root, PathBuf::from("C:\\dbgflow"));
        assert!(config.remove_install_files);
    }

    #[test]
    fn service_process_uses_default_service_name() {
        let config = parse_service_process_options_with_env(
            [
                OsString::from("--data-dir"),
                OsString::from("C:\\dbgflow\\var"),
            ],
            &env(&[("HTTP_PROXY", "")]),
        )
        .expect("parse service process options");

        assert_eq!(config.service_name, SERVICE_NAME);
    }

    #[test]
    fn remove_install_files_target_stays_under_install_root() {
        let target = remove_install_files_target(PathBuf::from("C:\\dbgflow"))
            .expect("resolve remove target");

        assert_eq!(target, PathBuf::from("C:\\dbgflow\\bin"));
    }
}
