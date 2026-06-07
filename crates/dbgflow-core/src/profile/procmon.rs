use super::{
    CollectorFactory, CollectorStart, CollectorStop, ProfileCollector, ProfileCollectorConfig,
    ProfileCollectorKind,
};
use crate::artifacts::{ArtifactKind, ArtifactRef};
use crate::{DbgFlowError, Result};
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

#[derive(Clone)]
pub struct ProcmonCollectorFactory {
    runtime: ProcmonRuntime,
    runner: Arc<dyn ProcmonCommandRunner>,
}

impl ProcmonCollectorFactory {
    pub fn new(runtime: ProcmonRuntime) -> Self {
        Self {
            runtime,
            runner: Arc::new(ProcessProcmonCommandRunner),
        }
    }
}

impl CollectorFactory for ProcmonCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        let ProfileCollectorConfig::Procmon {
            capture_stacks,
            filters,
        } = config
        else {
            return Err(DbgFlowError::Backend(
                "unsupported Procmon profile collector configuration".to_string(),
            ));
        };
        let procmon_exe = self.runtime.procmon_exe()?;
        Ok(Box::new(ProcmonCollector {
            procmon_exe,
            pml_path: output_dir.join("capture.pml"),
            summary_path: output_dir.join("summary.json"),
            capture_stacks: *capture_stacks,
            filters: filters.clone(),
            runner: self.runner.clone(),
        }))
    }
}

struct ProcmonCollector {
    procmon_exe: PathBuf,
    pml_path: PathBuf,
    summary_path: PathBuf,
    capture_stacks: bool,
    filters: super::ProcmonFilterConfig,
    runner: Arc<dyn ProcmonCommandRunner>,
}

impl ProfileCollector for ProcmonCollector {
    fn name(&self) -> &str {
        "procmon"
    }

    fn kind(&self) -> ProfileCollectorKind {
        ProfileCollectorKind::Procmon
    }

    fn start(&self) -> Result<CollectorStart> {
        self.runner.run(
            &self.procmon_exe,
            &[
                OsString::from("/AcceptEula"),
                OsString::from("/Quiet"),
                OsString::from("/Minimized"),
                OsString::from("/BackingFile"),
                self.pml_path.as_os_str().to_os_string(),
            ],
        )?;
        Ok(CollectorStart {
            warnings: procmon_warnings(self.capture_stacks),
        })
    }

    fn stop(&self) -> Result<CollectorStop> {
        self.runner.run(
            &self.procmon_exe,
            &[OsString::from("/AcceptEula"), OsString::from("/Terminate")],
        )?;
        self.write_summary()?;
        Ok(CollectorStop {
            artifacts: vec![
                ArtifactRef {
                    kind: ArtifactKind::ProfileCollectorTrace,
                    path: self.pml_path.clone(),
                },
                ArtifactRef {
                    kind: ArtifactKind::ProfileCollectorSummary,
                    path: self.summary_path.clone(),
                },
            ],
            warnings: Vec::new(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        let _ = self.runner.run(
            &self.procmon_exe,
            &[OsString::from("/AcceptEula"), OsString::from("/Terminate")],
        );
        Ok(())
    }
}

impl ProcmonCollector {
    fn write_summary(&self) -> Result<()> {
        let summary = json!({
            "kind": "procmon",
            "pml": self.pml_path,
            "capture_stacks_requested": self.capture_stacks,
            "filters": self.filters,
            "note": "capture.pml is the authoritative Procmon artifact; event export is added in a later collector version"
        });
        let text = serde_json::to_string_pretty(&summary)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        std::fs::write(&self.summary_path, text)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))
    }
}

trait ProcmonCommandRunner: Send + Sync {
    fn run(&self, exe: &Path, args: &[OsString]) -> Result<()>;
}

struct ProcessProcmonCommandRunner;

impl ProcmonCommandRunner for ProcessProcmonCommandRunner {
    fn run(&self, exe: &Path, args: &[OsString]) -> Result<()> {
        let output = std::process::Command::new(exe)
            .args(args)
            .output()
            .map_err(|error| DbgFlowError::Backend(format!("start procmon: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(DbgFlowError::Backend(format!(
            "procmon failed with exit code {:?}: {}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn procmon_warnings(capture_stacks: bool) -> Vec<String> {
    if capture_stacks {
        vec!["procmon stack capture depends on local Procmon configuration and symbols".to_string()]
    } else {
        Vec::new()
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
