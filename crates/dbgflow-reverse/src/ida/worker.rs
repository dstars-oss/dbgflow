use super::dynamic::DynamicIdaApi;
use super::install::{validate_ida_install_dir, IdaInstall, IDA_INSTALL_ENV};
use super::model::{FunctionInfo, IdaInfo, SegmentInfo};
use super::target::IdaTarget;
use dbgflow_common::SessionId;
use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const REVERSE_WORKER_COMMAND: &str = "worker";
pub const REVERSE_WORKER_KIND_IDA: &str = "reverse-ida";

const EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenIdaDatabase {
    pub install_dir: PathBuf,
    pub target: IdaTarget,
    pub run_auto_analysis: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenIdaDatabaseResult {
    pub ida: IdaInfo,
    pub warnings: Vec<String>,
}

pub trait ReverseWorkerLauncher: Send + Sync {
    fn spawn(
        &self,
        session_id: SessionId,
        install: IdaInstall,
        worker_log_path: PathBuf,
    ) -> Result<Arc<dyn ReverseWorker>>;
}

pub trait ReverseWorker: Send + Sync {
    fn open_database(&self, request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult>;
    fn list_segments(&self) -> Result<Vec<SegmentInfo>>;
    fn list_functions(&self) -> Result<Vec<FunctionInfo>>;
    fn has_exited(&self) -> Result<bool>;
    fn close(&self) -> Result<()>;
    fn kill(&self, reason: &str) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct ProcessReverseWorkerLauncher {
    executable: Option<PathBuf>,
}

impl ProcessReverseWorkerLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(executable.into()),
        }
    }
}

impl ReverseWorkerLauncher for ProcessReverseWorkerLauncher {
    fn spawn(
        &self,
        session_id: SessionId,
        install: IdaInstall,
        worker_log_path: PathBuf,
    ) -> Result<Arc<dyn ReverseWorker>> {
        let executable = match &self.executable {
            Some(executable) => executable.clone(),
            None => std::env::current_exe().map_err(|error| {
                DbgFlowError::Backend(format!("resolve reverse worker executable failed: {error}"))
            })?,
        };
        Ok(Arc::new(ProcessReverseWorker::spawn(
            session_id,
            executable,
            install,
            worker_log_path,
        )?))
    }
}

#[derive(Clone)]
struct ProcessReverseWorker {
    inner: Arc<ProcessReverseWorkerInner>,
}

struct ProcessReverseWorkerInner {
    _session_id: SessionId,
    pid: u32,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<std::io::BufReader<ChildStdout>>,
    request_lock: Mutex<()>,
    next_request_id: AtomicU64,
}

impl Drop for ProcessReverseWorkerInner {
    fn drop(&mut self) {
        let Ok(child) = self.child.get_mut() else {
            return;
        };
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ProcessReverseWorker {
    fn spawn(
        session_id: SessionId,
        executable: PathBuf,
        install: IdaInstall,
        worker_log_path: PathBuf,
    ) -> Result<Self> {
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&worker_log_path)
            .map_err(|error| {
                DbgFlowError::Artifact(format!(
                    "open reverse worker log {}: {error}",
                    worker_log_path.display()
                ))
            })?;
        let mut command = Command::new(&executable);
        command
            .arg(REVERSE_WORKER_COMMAND)
            .arg(REVERSE_WORKER_KIND_IDA)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        apply_ida_environment(&mut command, &install.install_dir)?;
        let mut child = command.spawn().map_err(|error| {
            DbgFlowError::Backend(format!(
                "spawn reverse worker {} failed: {error}",
                executable.display()
            ))
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            DbgFlowError::Backend("reverse worker stdin was not captured".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            DbgFlowError::Backend("reverse worker stdout was not captured".to_string())
        })?;

        Ok(Self {
            inner: Arc::new(ProcessReverseWorkerInner {
                _session_id: session_id,
                pid,
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(std::io::BufReader::new(stdout)),
                request_lock: Mutex::new(()),
                next_request_id: AtomicU64::new(0),
            }),
        })
    }

    fn request<T>(&self, request: WorkerRequest) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let _guard = self.inner.request_lock.lock().map_err(|_| {
            DbgFlowError::Backend("reverse worker request lock poisoned".to_string())
        })?;
        if self.has_exited()? {
            return Err(DbgFlowError::Backend(format!(
                "reverse worker process already exited: pid {}",
                self.inner.pid
            )));
        }

        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let input = WorkerInput {
            request_id,
            request,
        };
        {
            let mut stdin = self.inner.stdin.lock().map_err(|_| {
                DbgFlowError::Backend("reverse worker stdin lock poisoned".to_string())
            })?;
            serde_json::to_writer(&mut *stdin, &input).map_err(|error| {
                DbgFlowError::Backend(format!("write reverse worker request: {error}"))
            })?;
            writeln!(&mut *stdin).map_err(|error| {
                DbgFlowError::Backend(format!("write reverse worker request newline: {error}"))
            })?;
            stdin.flush().map_err(|error| {
                DbgFlowError::Backend(format!("flush reverse worker request: {error}"))
            })?;
        }

        loop {
            let mut line = String::new();
            let read = self
                .inner
                .stdout
                .lock()
                .map_err(|_| {
                    DbgFlowError::Backend("reverse worker stdout lock poisoned".to_string())
                })?
                .read_line(&mut line)
                .map_err(|error| {
                    DbgFlowError::Backend(format!("read reverse worker response: {error}"))
                })?;
            if read == 0 {
                return Err(DbgFlowError::Backend(format!(
                    "reverse worker exited before responding to request {request_id}"
                )));
            }
            let output: WorkerOutput = serde_json::from_str(&line).map_err(|error| {
                DbgFlowError::Backend(format!("parse reverse worker response: {error}: {line}"))
            })?;
            if output.request_id != request_id {
                continue;
            }
            if !output.ok {
                return Err(DbgFlowError::Backend(
                    output
                        .error
                        .unwrap_or_else(|| "reverse worker request failed".to_string()),
                ));
            }
            let result = output.result.unwrap_or(Value::Null);
            return serde_json::from_value(result).map_err(|error| {
                DbgFlowError::Backend(format!("decode reverse worker result: {error}"))
            });
        }
    }
}

impl ReverseWorker for ProcessReverseWorker {
    fn open_database(&self, request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult> {
        self.request(WorkerRequest::OpenDatabase(request))
    }

    fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
        self.request(WorkerRequest::ListSegments)
    }

    fn list_functions(&self) -> Result<Vec<FunctionInfo>> {
        self.request(WorkerRequest::ListFunctions)
    }

    fn has_exited(&self) -> Result<bool> {
        let mut child =
            self.inner.child.lock().map_err(|_| {
                DbgFlowError::Backend("reverse worker child lock poisoned".to_string())
            })?;
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| DbgFlowError::Backend(format!("poll reverse worker: {error}")))
    }

    fn close(&self) -> Result<()> {
        let result: EmptyResult = self.request(WorkerRequest::Close)?;
        let _ = result;
        let deadline = std::time::Instant::now() + EXIT_WAIT_TIMEOUT;
        loop {
            if self.has_exited()? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return self.kill("reverse_worker_close_timeout");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill(&self, _reason: &str) -> Result<()> {
        let mut child =
            self.inner.child.lock().map_err(|_| {
                DbgFlowError::Backend("reverse worker child lock poisoned".to_string())
            })?;
        if child
            .try_wait()
            .map_err(|error| DbgFlowError::Backend(format!("poll reverse worker: {error}")))?
            .is_none()
        {
            child
                .kill()
                .map_err(|error| DbgFlowError::Backend(format!("kill reverse worker: {error}")))?;
            let _ = child.wait();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerInput {
    request_id: u64,
    request: WorkerRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
enum WorkerRequest {
    OpenDatabase(OpenIdaDatabase),
    ListSegments,
    ListFunctions,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerOutput {
    request_id: u64,
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmptyResult {}

pub fn run_reverse_ida_worker_stdio(
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    let mut runtime = WorkerRuntime::default();
    for line in input.lines() {
        let line = line?;
        let parsed = serde_json::from_str::<WorkerInput>(&line);
        let response = match parsed {
            Ok(input) => runtime.handle(input),
            Err(error) => WorkerOutput {
                request_id: 0,
                ok: false,
                result: None,
                error: Some(format!("parse worker request: {error}")),
            },
        };
        serde_json::to_writer(&mut output, &response)?;
        writeln!(&mut output)?;
        output.flush()?;
        if matches!(response.result, Some(Value::Object(ref object)) if object.is_empty())
            && runtime.should_exit
        {
            break;
        }
    }
    Ok(())
}

#[derive(Default)]
struct WorkerRuntime {
    api: Option<DynamicIdaApi>,
    database_open: bool,
    should_exit: bool,
}

impl WorkerRuntime {
    fn handle(&mut self, input: WorkerInput) -> WorkerOutput {
        match self.handle_request(input.request) {
            Ok(result) => WorkerOutput {
                request_id: input.request_id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => WorkerOutput {
                request_id: input.request_id,
                ok: false,
                result: None,
                error: Some(error.to_string()),
            },
        }
    }

    fn handle_request(&mut self, request: WorkerRequest) -> Result<Value> {
        match request {
            WorkerRequest::OpenDatabase(request) => self.open_database(request),
            WorkerRequest::ListSegments => {
                let api = self.require_api()?;
                serde_json::to_value(api.list_segments()?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::ListFunctions => {
                let api = self.require_api()?;
                serde_json::to_value(api.list_functions()?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::Close => {
                if let Some(api) = &self.api {
                    if self.database_open {
                        api.close_database(false);
                        self.database_open = false;
                    }
                }
                self.should_exit = true;
                serde_json::to_value(EmptyResult {})
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
        }
    }

    fn open_database(&mut self, request: OpenIdaDatabase) -> Result<Value> {
        if self.api.is_some() {
            return Err(DbgFlowError::Backend(
                "IDA worker already has an open database".to_string(),
            ));
        }
        let install = validate_ida_install_dir(&request.install_dir)?;
        let api = DynamicIdaApi::load_and_initialize(&install)?;
        api.open_database(
            &request.target.path().to_string_lossy(),
            request.run_auto_analysis,
        )?;
        let result = OpenIdaDatabaseResult {
            ida: api.info(),
            warnings: Vec::new(),
        };
        self.database_open = true;
        self.api = Some(api);
        serde_json::to_value(result).map_err(|error| DbgFlowError::Backend(error.to_string()))
    }

    fn require_api(&self) -> Result<&DynamicIdaApi> {
        self.api.as_ref().ok_or_else(|| {
            DbgFlowError::Backend("IDA worker does not have an open database".to_string())
        })
    }
}

fn apply_ida_environment(command: &mut Command, install_dir: &Path) -> Result<()> {
    command.env(IDA_INSTALL_ENV, install_dir);
    let mut path_entries = vec![install_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(path_entries).map_err(|error| {
        DbgFlowError::Backend(format!("construct reverse worker PATH: {error}"))
    })?;
    command.env("PATH", joined);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn worker_response_reports_parse_error() {
        let mut out = Vec::new();
        run_reverse_ida_worker_stdio(Cursor::new("{not-json}\n"), &mut out)
            .expect("worker returns parse error");
        let text = String::from_utf8(out).expect("utf8");

        assert!(text.contains("parse worker request"));
    }

    #[test]
    fn worker_request_round_trips_open_shape() {
        let input = WorkerInput {
            request_id: 7,
            request: WorkerRequest::OpenDatabase(OpenIdaDatabase {
                install_dir: PathBuf::from(r"C:\Program Files\IDA Professional 9.3"),
                target: IdaTarget::Binary {
                    path: PathBuf::from(r"C:\sample.exe"),
                },
                run_auto_analysis: true,
            }),
        };

        let encoded = serde_json::to_string(&input).expect("encode");
        let decoded: WorkerInput = serde_json::from_str(&encoded).expect("decode");

        assert_eq!(decoded.request_id, 7);
    }
}
