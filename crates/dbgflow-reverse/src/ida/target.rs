use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum IdaTarget {
    Binary { path: PathBuf },
    Database { path: PathBuf },
}

impl IdaTarget {
    pub fn path(&self) -> &Path {
        match self {
            Self::Binary { path } | Self::Database { path } => path,
        }
    }

    fn with_path(self, path: PathBuf) -> Self {
        match self {
            Self::Binary { .. } => Self::Binary { path },
            Self::Database { .. } => Self::Database { path },
        }
    }
}

pub fn validate_ida_target(target: IdaTarget) -> Result<IdaTarget> {
    let raw_path = target.path();
    validate_path_text(raw_path)?;
    let canonical = raw_path.canonicalize().map_err(|error| {
        DbgFlowError::Backend(format!(
            "invalid IDA target path {}: {error}",
            raw_path.display()
        ))
    })?;
    if !canonical.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "invalid IDA target path {}; expected an existing file",
            canonical.display()
        )));
    }
    if matches!(target, IdaTarget::Database { .. }) && !is_ida_database_path(&canonical) {
        return Err(DbgFlowError::Backend(format!(
            "invalid IDA database path {}; expected .idb or .i64",
            canonical.display()
        )));
    }
    Ok(target.with_path(canonical))
}

fn validate_path_text(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(DbgFlowError::Backend(
            "IDA target path must not be empty".to_string(),
        ));
    }
    let text = path.to_string_lossy();
    if text.contains('\0') {
        return Err(DbgFlowError::Backend(
            "IDA target path must not contain NUL bytes".to_string(),
        ));
    }
    Ok(())
}

fn is_ida_database_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("idb") || extension.eq_ignore_ascii_case("i64")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_binary_file() {
        let root = test_dir("binary");
        let binary = root.join("sample.exe");
        std::fs::write(&binary, b"MZ").expect("write binary");

        let validated =
            validate_ida_target(IdaTarget::Binary { path: binary }).expect("validate target");

        assert!(validated.path().is_absolute());
    }

    #[test]
    fn rejects_missing_target() {
        let error = validate_ida_target(IdaTarget::Binary {
            path: PathBuf::from(r"C:\does-not-exist\missing.exe"),
        })
        .expect_err("reject missing");

        assert!(error.to_string().contains("invalid IDA target path"));
    }

    #[test]
    fn rejects_database_without_idb_extension() {
        let root = test_dir("database-extension");
        let file = root.join("sample.exe");
        std::fs::write(&file, b"MZ").expect("write file");

        let error = validate_ida_target(IdaTarget::Database { path: file })
            .expect_err("reject non-database");

        assert!(error.to_string().contains("expected .idb or .i64"));
    }

    fn test_dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dbgflow-ida-target-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test dir");
        root
    }
}
