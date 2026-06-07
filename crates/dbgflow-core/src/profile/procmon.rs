use crate::{DbgFlowError, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcmonRuntime {
    sysinternals_dir: Option<PathBuf>,
}

impl ProcmonRuntime {
    pub fn unavailable() -> Self {
        Self {
            sysinternals_dir: None,
        }
    }

    pub fn with_sysinternals_dir(path: PathBuf) -> Self {
        Self {
            sysinternals_dir: Some(path),
        }
    }

    pub fn sysinternals_dir(&self) -> Option<&std::path::Path> {
        self.sysinternals_dir.as_deref()
    }

    pub fn procmon_exe(&self) -> Result<PathBuf> {
        let dir = self.sysinternals_dir.as_ref().ok_or_else(|| {
            DbgFlowError::Backend(
                "procmon collector requires service --sysinternals-dir".to_string(),
            )
        })?;
        for name in ["Procmon64.exe", "Procmon.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(DbgFlowError::Backend(format!(
            "procmon collector requires Procmon64.exe or Procmon.exe under {}",
            dir.display()
        )))
    }
}

impl From<Option<PathBuf>> for ProcmonRuntime {
    fn from(value: Option<PathBuf>) -> Self {
        match value {
            Some(path) => Self::with_sysinternals_dir(path),
            None => Self::unavailable(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProcmonRuntime;

    #[test]
    fn procmon_runtime_prefers_procmon64() {
        let root =
            std::env::temp_dir().join(format!("dbgflow-procmon-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(root.join("Procmon.exe"), b"exe").expect("write procmon");
        std::fs::write(root.join("Procmon64.exe"), b"exe").expect("write procmon64");

        let runtime = ProcmonRuntime::with_sysinternals_dir(root.clone());

        assert_eq!(
            runtime.procmon_exe().expect("resolve procmon"),
            root.join("Procmon64.exe")
        );
    }

    #[test]
    fn procmon_runtime_without_sysinternals_dir_is_unavailable() {
        let error = ProcmonRuntime::unavailable()
            .procmon_exe()
            .expect_err("procmon unavailable");

        assert!(error.to_string().contains("--sysinternals-dir"));
    }
}
