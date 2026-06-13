use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DBGFLOW_IDA_DIR_ENV: &str = "DBGFLOW_IDA_DIR";
pub const IDA_INSTALL_ENV: &str = "IDADIR";
const DEFAULT_WINDOWS_IDA_DIR: &str = r"C:\Program Files\IDA Professional 9.3";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdaRuntimeConfig {
    pub install_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdaInstall {
    pub install_dir: PathBuf,
    pub ida_dll: PathBuf,
    pub idalib_dll: PathBuf,
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
            root.canonicalize().expect("canonicalize")
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

    fn test_dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dbgflow-ida-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test dir");
        root
    }
}
