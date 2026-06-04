use crate::mcp::default_artifact_root;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

pub const DEFAULT_BIND: &str = "127.0.0.1:7331";
pub const SERVICE_NAME: &str = "dbgflow-mcp";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: SocketAddr,
    pub artifact_root: PathBuf,
    pub log_dir: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("valid default bind address"),
            artifact_root: default_artifact_root(),
            log_dir: None,
        }
    }
}

pub fn parse_options<I>(args: I) -> Result<AppConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut config = AppConfig::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;

        if let Some(value) = arg.strip_prefix("--bind=") {
            config.bind = parse_bind(value)?;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--artifact-root=") {
            config.artifact_root = PathBuf::from(value);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--log-dir=") {
            config.log_dir = Some(PathBuf::from(value));
            continue;
        }

        match arg.as_str() {
            "--bind" => {
                let value = next_value(&mut args, "--bind")?;
                config.bind = parse_bind(&value)?;
            }
            "--artifact-root" => {
                let value = next_value(&mut args, "--artifact-root")?;
                config.artifact_root = PathBuf::from(value);
            }
            "--log-dir" => {
                let value = next_value(&mut args, "--log-dir")?;
                config.log_dir = Some(PathBuf::from(value));
            }
            "--help" | "-h" => return Err(help_text().to_string()),
            other => return Err(format!("unknown option: {other}\n\n{}", help_text())),
        }
    }

    Ok(config)
}

pub fn help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp                         Run stdio MCP transport\n  dbgflow-mcp http [options]           Run Streamable HTTP MCP transport\n  dbgflow-mcp service [options]        Run as a Windows service\n\nOptions:\n  --bind <addr:port>                   Default: 127.0.0.1:7331\n  --artifact-root <path>               Default: workspace artifacts or DBGFLOW_ARTIFACT_ROOT\n  --log-dir <path>                     Service log directory"
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

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    let bind: SocketAddr = value
        .parse()
        .map_err(|error| format!("invalid bind address {value}: {error}"))?;
    if !bind.ip().is_loopback() {
        return Err(format!(
            "bind address must be loopback because HTTP transport has no authentication: {value}"
        ));
    }
    Ok(bind)
}

#[cfg(test)]
mod tests {
    use super::{parse_options, DEFAULT_BIND};
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn parses_runtime_options() {
        let config = parse_options([
            OsString::from("--bind"),
            OsString::from("127.0.0.1:9000"),
            OsString::from("--artifact-root=C:\\dbgflow\\artifacts"),
            OsString::from("--log-dir"),
            OsString::from("C:\\dbgflow\\logs"),
        ])
        .expect("parse options");

        assert_eq!(config.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(
            config.artifact_root,
            PathBuf::from("C:\\dbgflow\\artifacts")
        );
        assert_eq!(config.log_dir, Some(PathBuf::from("C:\\dbgflow\\logs")));
    }

    #[test]
    fn uses_default_bind() {
        let config = parse_options([]).expect("parse empty options");
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let error = parse_options([OsString::from("--bind"), OsString::from("0.0.0.0:7331")])
            .expect_err("reject non-loopback bind");

        assert!(error.contains("loopback"));
    }
}
