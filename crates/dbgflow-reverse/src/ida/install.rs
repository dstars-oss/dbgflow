use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DBGFLOW_IDA_DIR_ENV: &str = "DBGFLOW_IDA_DIR";
pub const DBGFLOW_IDA_PYTHON_ENV: &str = "DBGFLOW_IDA_PYTHON";
pub const DBGFLOW_IDA_PRO_MCP_SRC_ENV: &str = "DBGFLOW_IDA_PRO_MCP_SRC";
pub const IDA_INSTALL_ENV: &str = "IDADIR";
const DEFAULT_WINDOWS_IDA_DIR: &str = r"C:\Program Files\IDA Professional 9.3";
const DEFAULT_MAX_WORKERS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdaRuntimeConfig {
    pub install_dir: Option<PathBuf>,
    pub python_executable: Option<PathBuf>,
    pub vendor_src_dir: Option<PathBuf>,
    pub max_workers: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaInstall {
    pub install_dir: PathBuf,
    pub ida_dll: PathBuf,
    pub idalib_dll: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdaSupervisorRuntime {
    pub install: IdaInstall,
    pub python_executable: PathBuf,
    pub vendor_src_dir: PathBuf,
    pub max_workers: usize,
}

pub fn resolve_ida_install(config: &IdaRuntimeConfig) -> Result<IdaInstall> {
    if let Some(path) = &config.install_dir {
        return validate_ida_install_dir(path);
    }
    if let Some(path) = std::env::var_os(DBGFLOW_IDA_DIR_ENV).map(PathBuf::from) {
        return validate_ida_install_dir(&path);
    }
    validate_ida_install_dir(Path::new(DEFAULT_WINDOWS_IDA_DIR))
}

pub fn resolve_supervisor_runtime(config: &IdaRuntimeConfig) -> Result<IdaSupervisorRuntime> {
    Ok(IdaSupervisorRuntime {
        install: resolve_ida_install(config)?,
        python_executable: resolve_python_executable(config),
        vendor_src_dir: resolve_vendor_src_dir(config)?,
        max_workers: config.max_workers.unwrap_or(DEFAULT_MAX_WORKERS),
    })
}

pub fn validate_ida_install_dir(path: &Path) -> Result<IdaInstall> {
    if path.as_os_str().is_empty() {
        return Err(DbgFlowError::Backend(
            "IDA install dir must not be empty".to_string(),
        ));
    }
    let install_dir = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                DbgFlowError::Backend(format!("resolve current directory for IDA dir: {error}"))
            })?
            .join(path)
    };
    let install_dir = install_dir.canonicalize().map_err(|error| {
        DbgFlowError::Backend(format!(
            "invalid IDA install dir {}: {error}",
            install_dir.display()
        ))
    })?;
    let install_dir = normalize_ida_runtime_path(install_dir);
    if !install_dir.is_dir() {
        return Err(DbgFlowError::Backend(format!(
            "invalid IDA install dir {}; expected a directory",
            install_dir.display()
        )));
    }

    for file_name in ["ida.exe", "ida.dll", "idalib.dll", "ida.hlp"] {
        let path = install_dir.join(file_name);
        if !path.is_file() {
            return Err(DbgFlowError::Backend(format!(
                "invalid IDA install dir {}; missing {file_name}",
                install_dir.display()
            )));
        }
    }

    Ok(IdaInstall {
        ida_dll: install_dir.join("ida.dll"),
        idalib_dll: install_dir.join("idalib.dll"),
        install_dir,
    })
}

fn resolve_python_executable(config: &IdaRuntimeConfig) -> PathBuf {
    config
        .python_executable
        .clone()
        .or_else(|| std::env::var_os(DBGFLOW_IDA_PYTHON_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("python"))
}

fn resolve_vendor_src_dir(config: &IdaRuntimeConfig) -> Result<PathBuf> {
    let candidates = config
        .vendor_src_dir
        .clone()
        .into_iter()
        .chain(std::env::var_os(DBGFLOW_IDA_PRO_MCP_SRC_ENV).map(PathBuf::from))
        .chain(current_exe_vendor_src_dir())
        .chain(workspace_vendor_src_dir())
        .collect::<Vec<_>>();

    for candidate in candidates {
        let resolved = absolutize_path(&candidate)?;
        if resolved
            .join("ida_pro_mcp")
            .join("idalib_supervisor.py")
            .is_file()
        {
            return Ok(resolved);
        }
    }

    Err(DbgFlowError::Backend(
        "ida-pro-mcp vendored src directory was not found; set DBGFLOW_IDA_PRO_MCP_SRC".to_string(),
    ))
}

fn current_exe_vendor_src_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|dir| dir.join("vendor").join("ida-pro-mcp").join("src"))
}

fn workspace_vendor_src_dir() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("vendor").join("ida-pro-mcp").join("src"))
}

fn absolutize_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| DbgFlowError::Backend(format!("resolve current dir: {error}")))?
            .join(path))
    }
}

pub(crate) fn normalize_ida_runtime_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_install_dir_with_required_files() {
        let root = test_dir("valid");
        for file_name in ["ida.exe", "ida.dll", "idalib.dll", "ida.hlp"] {
            std::fs::write(root.join(file_name), b"").expect("write required file");
        }

        let install = validate_ida_install_dir(&root).expect("validate install");

        assert_eq!(
            install.install_dir,
            normalize_ida_runtime_path(root.canonicalize().expect("canonicalize"))
        );
    }

    #[test]
    fn rejects_install_dir_missing_idalib() {
        let root = test_dir("missing-idalib");
        for file_name in ["ida.exe", "ida.dll", "ida.hlp"] {
            std::fs::write(root.join(file_name), b"").expect("write required file");
        }

        let error = validate_ida_install_dir(&root).expect_err("reject missing idalib");

        assert!(error.to_string().contains("missing idalib.dll"));
    }

    #[test]
    fn strips_windows_verbatim_paths_for_ida_runtime() {
        assert_eq!(
            normalize_ida_runtime_path(PathBuf::from(r"\\?\C:\IDA\sample.exe")),
            PathBuf::from(r"C:\IDA\sample.exe")
        );
        assert_eq!(
            normalize_ida_runtime_path(PathBuf::from(r"\\?\UNC\server\share\sample.exe")),
            PathBuf::from(r"\\server\share\sample.exe")
        );
    }

    fn test_dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dbgflow-ida-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test dir");
        root
    }
}
