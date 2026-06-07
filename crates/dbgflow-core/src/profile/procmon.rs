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
            csv_path: output_dir.join("events.csv"),
            jsonl_path: output_dir.join("events.jsonl"),
            stack_xml_path: output_dir.join("events.xml"),
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
    csv_path: PathBuf,
    jsonl_path: PathBuf,
    stack_xml_path: PathBuf,
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

    fn stop(&self, target_pid: Option<u32>) -> Result<CollectorStop> {
        self.runner.run(
            &self.procmon_exe,
            &[OsString::from("/AcceptEula"), OsString::from("/Terminate")],
        )?;
        self.export_csv()?;
        if self.capture_stacks {
            self.export_stack_xml()?;
        }
        let exported_events = self.write_filtered_events(target_pid)?;
        self.write_summary(target_pid, exported_events)?;
        let mut artifacts = vec![
            ArtifactRef {
                kind: ArtifactKind::ProfileCollectorTrace,
                path: self.pml_path.clone(),
            },
            ArtifactRef {
                kind: ArtifactKind::ProfileCollectorEvents,
                path: self.csv_path.clone(),
            },
            ArtifactRef {
                kind: ArtifactKind::ProfileCollectorEvents,
                path: self.jsonl_path.clone(),
            },
            ArtifactRef {
                kind: ArtifactKind::ProfileCollectorSummary,
                path: self.summary_path.clone(),
            },
        ];
        if self.capture_stacks {
            artifacts.push(ArtifactRef {
                kind: ArtifactKind::ProfileCollectorEvents,
                path: self.stack_xml_path.clone(),
            });
        }
        Ok(CollectorStop {
            artifacts,
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
    fn export_csv(&self) -> Result<()> {
        self.runner.run(
            &self.procmon_exe,
            &[
                OsString::from("/AcceptEula"),
                OsString::from("/Quiet"),
                OsString::from("/OpenLog"),
                self.pml_path.as_os_str().to_os_string(),
                OsString::from("/SaveAs"),
                self.csv_path.as_os_str().to_os_string(),
            ],
        )
    }

    fn export_stack_xml(&self) -> Result<()> {
        self.runner.run(
            &self.procmon_exe,
            &[
                OsString::from("/AcceptEula"),
                OsString::from("/Quiet"),
                OsString::from("/OpenLog"),
                self.pml_path.as_os_str().to_os_string(),
                OsString::from("/SaveAs1"),
                self.stack_xml_path.as_os_str().to_os_string(),
            ],
        )
    }

    fn write_filtered_events(&self, target_pid: Option<u32>) -> Result<usize> {
        let csv = std::fs::read_to_string(&self.csv_path)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        let mut lines = csv.lines();
        let Some(header_line) = lines.next() else {
            std::fs::write(&self.jsonl_path, b"")
                .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
            return Ok(0);
        };
        let headers = parse_csv_line(header_line);
        let pid_index = header_index(&headers, "PID");
        let operation_index = header_index(&headers, "Operation");
        let path_index = header_index(&headers, "Path");
        let mut output = String::new();
        let mut count = 0usize;
        for line in lines {
            let columns = parse_csv_line(line);
            if !procmon_row_matches(
                &columns,
                pid_index,
                operation_index,
                path_index,
                target_pid,
                &self.filters,
            ) {
                continue;
            }
            let mut object = serde_json::Map::new();
            for (index, header) in headers.iter().enumerate() {
                object.insert(
                    header.clone(),
                    serde_json::Value::String(columns.get(index).cloned().unwrap_or_default()),
                );
            }
            output.push_str(
                &serde_json::to_string(&object)
                    .map_err(|error| DbgFlowError::Artifact(error.to_string()))?,
            );
            output.push('\n');
            count += 1;
        }
        std::fs::write(&self.jsonl_path, output)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(count)
    }

    fn write_summary(&self, target_pid: Option<u32>, exported_events: usize) -> Result<()> {
        let summary = json!({
            "kind": "procmon",
            "pml": self.pml_path,
            "events_csv": self.csv_path,
            "events_jsonl": self.jsonl_path,
            "stack_xml": if self.capture_stacks { Some(self.stack_xml_path.clone()) } else { None },
            "capture_stacks_requested": self.capture_stacks,
            "filters": self.filters,
            "target_pid": target_pid,
            "exported_events": exported_events,
            "note": "capture.pml is the authoritative Procmon artifact; events.jsonl is a best-effort target/path/operation filtered export from events.csv"
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

fn header_index(headers: &[String], name: &str) -> Option<usize> {
    headers
        .iter()
        .position(|header| header.eq_ignore_ascii_case(name))
}

fn procmon_row_matches(
    columns: &[String],
    pid_index: Option<usize>,
    operation_index: Option<usize>,
    path_index: Option<usize>,
    target_pid: Option<u32>,
    filters: &super::ProcmonFilterConfig,
) -> bool {
    if let (Some(target_pid), Some(pid_index)) = (target_pid, pid_index) {
        let row_pid = columns
            .get(pid_index)
            .and_then(|value| value.trim().parse::<u32>().ok());
        if row_pid != Some(target_pid) {
            return false;
        }
    }
    if !filters.operations.is_empty() {
        let Some(operation) = operation_index.and_then(|index| columns.get(index)) else {
            return false;
        };
        if !filters
            .operations
            .iter()
            .any(|expected| expected.eq_ignore_ascii_case(operation))
        {
            return false;
        }
    }
    if !filters.paths.is_empty() {
        let Some(path) = path_index.and_then(|index| columns.get(index)) else {
            return false;
        };
        let path = path.to_lowercase();
        if !filters.paths.iter().any(|expected| {
            let expected = expected.to_string_lossy().to_lowercase();
            path == expected || path.starts_with(&expected)
        }) {
            return false;
        }
    }
    true
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut value = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                value.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                values.push(value);
                value = String::new();
            }
            _ => value.push(ch),
        }
    }
    values.push(value);
    values
}

#[cfg(test)]
mod tests {
    use super::{
        parse_csv_line, procmon_row_matches, ProcmonCollector, ProcmonCommandRunner, ProcmonRuntime,
    };
    use crate::artifacts::ArtifactKind;
    use crate::profile::ProcmonFilterConfig;
    use crate::profile::ProfileCollector;
    use crate::Result;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    #[test]
    fn procmon_csv_parser_handles_quoted_commas() {
        let values = parse_csv_line("\"Time\",\"PID\",\"Path\",\"Detail\",\"a,b\"");
        assert_eq!(
            values,
            vec![
                "Time".to_string(),
                "PID".to_string(),
                "Path".to_string(),
                "Detail".to_string(),
                "a,b".to_string()
            ]
        );
    }

    #[test]
    fn procmon_row_filter_matches_pid_operation_and_path_prefix() {
        let filters = ProcmonFilterConfig {
            operations: vec!["ReadFile".to_string()],
            paths: vec![PathBuf::from("C:\\data")],
        };
        let row = vec![
            "read-file.exe".to_string(),
            "1234".to_string(),
            "ReadFile".to_string(),
            "C:\\data\\large.bin".to_string(),
        ];

        assert!(procmon_row_matches(
            &row,
            Some(1),
            Some(2),
            Some(3),
            Some(1234),
            &filters
        ));
        assert!(!procmon_row_matches(
            &row,
            Some(1),
            Some(2),
            Some(3),
            Some(5678),
            &filters
        ));
    }

    #[test]
    fn procmon_stop_exports_and_filters_events() {
        let root = std::env::temp_dir().join(format!(
            "dbgflow-procmon-export-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let collector = ProcmonCollector {
            procmon_exe: root.join("Procmon64.exe"),
            pml_path: root.join("capture.pml"),
            csv_path: root.join("events.csv"),
            jsonl_path: root.join("events.jsonl"),
            stack_xml_path: root.join("events.xml"),
            summary_path: root.join("summary.json"),
            capture_stacks: false,
            filters: ProcmonFilterConfig {
                operations: vec!["ReadFile".to_string()],
                paths: vec![PathBuf::from("C:\\data")],
            },
            runner: Arc::new(FakeProcmonRunner),
        };

        let stopped = collector.stop(Some(1234)).expect("stop collector");

        assert!(stopped.artifacts.iter().any(|artifact| artifact.kind
            == ArtifactKind::ProfileCollectorEvents
            && artifact.path.ends_with("events.jsonl")));
        let events = std::fs::read_to_string(root.join("events.jsonl")).expect("read jsonl");
        assert!(events.contains("ReadFile"));
        assert!(events.contains("C:\\\\data\\\\large.bin"));
        assert!(!events.contains("WriteFile"));
        let summary = std::fs::read_to_string(root.join("summary.json")).expect("read summary");
        assert!(summary.contains("\"target_pid\": 1234"));
        assert!(summary.contains("\"exported_events\": 1"));
    }

    struct FakeProcmonRunner;

    impl ProcmonCommandRunner for FakeProcmonRunner {
        fn run(&self, _exe: &std::path::Path, args: &[OsString]) -> Result<()> {
            if let Some(index) = args.iter().position(|arg| arg == "/SaveAs") {
                let csv_path = PathBuf::from(&args[index + 1]);
                std::fs::write(
                    csv_path,
                    concat!(
                        "\"Process Name\",\"PID\",\"Operation\",\"Path\"\n",
                        "\"read-file.exe\",\"1234\",\"ReadFile\",\"C:\\data\\large.bin\"\n",
                        "\"read-file.exe\",\"1234\",\"WriteFile\",\"C:\\data\\large.bin\"\n",
                        "\"read-file.exe\",\"5678\",\"ReadFile\",\"C:\\data\\other.bin\"\n",
                    ),
                )
                .expect("write fake csv");
            }
            Ok(())
        }
    }
}
