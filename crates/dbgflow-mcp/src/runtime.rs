use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

pub const DEFAULT_BIND: &str = "127.0.0.1:7331";
pub const SERVICE_NAME: &str = "dbgflow-mcp";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
}

pub fn parse_options<I>(args: I) -> Result<AppConfig, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut bind = DEFAULT_BIND.parse().expect("valid default bind address");
    let mut data_dir = None;
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

        match arg.as_str() {
            "--bind" => {
                let value = next_value(&mut args, "--bind")?;
                bind = parse_bind(&value)?;
            }
            "--data-dir" => {
                let value = next_value(&mut args, "--data-dir")?;
                data_dir = Some(PathBuf::from(value));
            }
            "--help" | "-h" => return Err(help_text().to_string()),
            other => return Err(format!("unknown option: {other}\n\n{}", help_text())),
        }
    }

    let data_dir =
        data_dir.ok_or_else(|| format!("missing required --data-dir <path>\n\n{}", help_text()))?;
    Ok(AppConfig { bind, data_dir })
}

pub fn help_text() -> &'static str {
    "Usage:\n  dbgflow-mcp http --data-dir <path> [options]     Run local HTTP MCP transport\n  dbgflow-mcp service --data-dir <path> [options]  Run as a Windows service\n  dbgflow-mcp worker session                       Run an internal session worker process\n\nOptions:\n  --bind <addr:port>                             Default: 127.0.0.1:7331\n  --data-dir <path>                              Required. Uses <path>\\artifacts and <path>\\logs"
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
            "bind address must be loopback; dbgflow does not support remote HTTP access: {value}"
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
            OsString::from("--data-dir=C:\\dbgflow\\var"),
        ])
        .expect("parse options");

        assert_eq!(config.bind.to_string(), "127.0.0.1:9000");
        assert_eq!(config.data_dir, PathBuf::from("C:\\dbgflow\\var"));
    }

    #[test]
    fn uses_default_bind_with_required_data_dir() {
        let config = parse_options([OsString::from("--data-dir"), OsString::from(".\\var")])
            .expect("parse options");
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(config.data_dir, PathBuf::from(".\\var"));
    }

    #[test]
    fn rejects_missing_data_dir() {
        let error = parse_options([]).expect_err("reject missing data dir");
        assert!(error.contains("--data-dir"));
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let error = parse_options([OsString::from("--bind"), OsString::from("0.0.0.0:7331")])
            .expect_err("reject non-loopback bind");

        assert!(error.contains("loopback"));
    }

    #[test]
    fn rejects_removed_directory_options() {
        let artifact_error =
            parse_options([OsString::from("--artifact-root=C:\\dbgflow\\artifacts")])
                .expect_err("reject artifact-root");
        assert!(artifact_error.contains("unknown option"));

        let log_error = parse_options([OsString::from("--log-dir"), OsString::from("C:\\logs")])
            .expect_err("reject log-dir");
        assert!(log_error.contains("unknown option"));
    }
}
