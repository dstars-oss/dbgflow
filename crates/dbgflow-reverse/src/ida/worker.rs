use super::dynamic::DynamicIdaApi;
use super::install::{validate_ida_install_dir, IdaInstall, IDA_INSTALL_ENV};
use super::model::{
    CloseDatabaseResult, DecompileRequest, DecompileResult, DisassembleRequest, Disassembly,
    ExportInfo, FunctionInfo, FunctionLookup, IdaInfo, IdaMetadata, ImportInfo, ListXrefsRequest,
    LookupFunctionsRequest, MutationItemResult, PageRequest, RenameRequest, SegmentInfo,
    SetCommentRequest, SetTypeRequest, StringInfo,
};
use super::target::IdaTarget;
use dbgflow_common::logging::LogSink;
use dbgflow_common::process::{
    log_process_launch, spawn_process, EnvChange, LaunchStdio, ManagedChild, ProcessLaunchContext,
    ProcessLaunchSpec,
};
use dbgflow_common::SessionId;
use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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
        launch_context: ProcessLaunchContext,
        logger: Arc<dyn LogSink>,
    ) -> Result<Arc<dyn ReverseWorker>>;
}

pub trait ReverseWorker: Send + Sync {
    fn open_database(&self, request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult>;
    fn get_metadata(&self) -> Result<IdaMetadata>;
    fn list_segments(&self) -> Result<Vec<SegmentInfo>>;
    fn list_functions(&self) -> Result<Vec<FunctionInfo>>;
    fn list_strings(&self, request: PageRequest) -> Result<Vec<StringInfo>>;
    fn list_imports(&self, request: PageRequest) -> Result<Vec<ImportInfo>>;
    fn list_exports(&self, request: PageRequest) -> Result<Vec<ExportInfo>>;
    fn lookup_functions(&self, request: LookupFunctionsRequest) -> Result<Vec<FunctionLookup>>;
    fn disassemble(&self, request: DisassembleRequest) -> Result<Disassembly>;
    fn decompile(&self, request: DecompileRequest) -> Result<DecompileResult>;
    fn list_xrefs(&self, request: ListXrefsRequest) -> Result<super::model::XrefsResult>;
    fn rename(&self, request: RenameRequest) -> Result<Vec<MutationItemResult>>;
    fn set_comment(&self, request: SetCommentRequest) -> Result<Vec<MutationItemResult>>;
    fn set_type(&self, request: SetTypeRequest) -> Result<Vec<MutationItemResult>>;
    fn has_exited(&self) -> Result<bool>;
    fn close(&self, save: bool) -> Result<CloseDatabaseResult>;
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
        launch_context: ProcessLaunchContext,
        logger: Arc<dyn LogSink>,
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
            launch_context,
            logger,
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
    child: Mutex<ManagedChild>,
    stdin: Mutex<File>,
    stdout: Mutex<BufReader<File>>,
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
        launch_context: ProcessLaunchContext,
        logger: Arc<dyn LogSink>,
    ) -> Result<Self> {
        let spec = reverse_worker_launch_spec(&executable, &install, &worker_log_path)?;
        let mut child = spawn_process(&spec, &launch_context).map_err(|error| {
            DbgFlowError::Backend(format!(
                "spawn reverse worker {} failed: {error}",
                executable.display()
            ))
        })?;
        let pid = child.pid();
        let stdin = child.take_stdin().ok_or_else(|| {
            DbgFlowError::Backend("reverse worker stdin was not captured".to_string())
        })?;
        let stdout = child.take_stdout().ok_or_else(|| {
            DbgFlowError::Backend("reverse worker stdout was not captured".to_string())
        })?;
        log_process_launch(
            &logger,
            "reverse_ida",
            "worker_process_launch_resolved",
            child.audit(),
        );

        Ok(Self {
            inner: Arc::new(ProcessReverseWorkerInner {
                _session_id: session_id,
                pid,
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(BufReader::new(stdout)),
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
                return Err(output.error.map_or_else(
                    || DbgFlowError::Backend("reverse worker request failed".to_string()),
                    worker_error_to_dbgflow,
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

    fn get_metadata(&self) -> Result<IdaMetadata> {
        self.request(WorkerRequest::GetMetadata)
    }

    fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
        self.request(WorkerRequest::ListSegments)
    }

    fn list_functions(&self) -> Result<Vec<FunctionInfo>> {
        self.request(WorkerRequest::ListFunctions)
    }

    fn list_strings(&self, request: PageRequest) -> Result<Vec<StringInfo>> {
        self.request(WorkerRequest::ListStrings(request))
    }

    fn list_imports(&self, request: PageRequest) -> Result<Vec<ImportInfo>> {
        self.request(WorkerRequest::ListImports(request))
    }

    fn list_exports(&self, request: PageRequest) -> Result<Vec<ExportInfo>> {
        self.request(WorkerRequest::ListExports(request))
    }

    fn lookup_functions(&self, request: LookupFunctionsRequest) -> Result<Vec<FunctionLookup>> {
        self.request(WorkerRequest::LookupFunctions(request))
    }

    fn disassemble(&self, request: DisassembleRequest) -> Result<Disassembly> {
        self.request(WorkerRequest::Disassemble(request))
    }

    fn decompile(&self, request: DecompileRequest) -> Result<DecompileResult> {
        self.request(WorkerRequest::Decompile(request))
    }

    fn list_xrefs(&self, request: ListXrefsRequest) -> Result<super::model::XrefsResult> {
        self.request(WorkerRequest::ListXrefs(request))
    }

    fn rename(&self, request: RenameRequest) -> Result<Vec<MutationItemResult>> {
        self.request(WorkerRequest::Rename(request))
    }

    fn set_comment(&self, request: SetCommentRequest) -> Result<Vec<MutationItemResult>> {
        self.request(WorkerRequest::SetComment(request))
    }

    fn set_type(&self, request: SetTypeRequest) -> Result<Vec<MutationItemResult>> {
        self.request(WorkerRequest::SetType(request))
    }

    fn has_exited(&self) -> Result<bool> {
        let mut child =
            self.inner.child.lock().map_err(|_| {
                DbgFlowError::Backend("reverse worker child lock poisoned".to_string())
            })?;
        child.try_wait().map(|status| status.is_some())
    }

    fn close(&self, save: bool) -> Result<CloseDatabaseResult> {
        let result: CloseDatabaseResult = self.request(WorkerRequest::Close { save })?;
        let deadline = std::time::Instant::now() + EXIT_WAIT_TIMEOUT;
        loop {
            if self.has_exited()? {
                return Ok(result);
            }
            if std::time::Instant::now() >= deadline {
                self.kill("reverse_worker_close_timeout")?;
                return Ok(result);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill(&self, _reason: &str) -> Result<()> {
        let mut child =
            self.inner.child.lock().map_err(|_| {
                DbgFlowError::Backend("reverse worker child lock poisoned".to_string())
            })?;
        if child.try_wait()?.is_none() {
            child.kill()?;
            let _ = child.wait();
        }
        Ok(())
    }
}

fn reverse_worker_launch_spec(
    executable: &Path,
    install: &IdaInstall,
    worker_log_path: &Path,
) -> Result<ProcessLaunchSpec> {
    let mut spec = ProcessLaunchSpec::new(executable);
    spec.args = vec![
        REVERSE_WORKER_COMMAND.into(),
        REVERSE_WORKER_KIND_IDA.into(),
    ];
    spec.env = ida_env_changes(&install.install_dir)?;
    spec.stdin = LaunchStdio::Piped;
    spec.stdout = LaunchStdio::Piped;
    spec.stderr = LaunchStdio::File(worker_log_path.to_path_buf());
    spec.hide_console_window();
    Ok(spec)
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
    GetMetadata,
    ListSegments,
    ListFunctions,
    ListStrings(PageRequest),
    ListImports(PageRequest),
    ListExports(PageRequest),
    LookupFunctions(LookupFunctionsRequest),
    Disassemble(DisassembleRequest),
    Decompile(DecompileRequest),
    ListXrefs(ListXrefsRequest),
    Rename(RenameRequest),
    SetComment(SetCommentRequest),
    SetType(SetTypeRequest),
    Close { save: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerOutput {
    request_id: u64,
    ok: bool,
    result: Option<Value>,
    error: Option<WorkerErrorInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkerErrorInfo {
    kind: String,
    message: String,
}

impl WorkerErrorInfo {
    fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    fn backend(message: impl Into<String>) -> Self {
        Self::new("backend", message)
    }
}

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
                error: Some(WorkerErrorInfo::backend(format!(
                    "parse worker request: {error}"
                ))),
            },
        };
        serde_json::to_writer(&mut output, &response)?;
        writeln!(&mut output)?;
        output.flush()?;
        if runtime.should_exit {
            break;
        }
    }
    Ok(())
}

#[derive(Default)]
struct WorkerRuntime {
    api: Option<DynamicIdaApi>,
    target: Option<IdaTarget>,
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
                error: Some(worker_error_from_dbgflow(error)),
            },
        }
    }

    fn handle_request(&mut self, request: WorkerRequest) -> Result<Value> {
        match request {
            WorkerRequest::OpenDatabase(request) => self.open_database(request),
            WorkerRequest::GetMetadata => {
                let api = self.require_api()?;
                let target = self.require_target()?;
                serde_json::to_value(api.metadata(target)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
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
            WorkerRequest::ListStrings(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.list_strings(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::ListImports(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.list_imports(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::ListExports(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.list_exports(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::LookupFunctions(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.lookup_functions(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::Disassemble(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.disassemble(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::Decompile(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.decompile(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::ListXrefs(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.list_xrefs(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::Rename(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.rename(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::SetComment(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.set_comment(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::SetType(request) => {
                let api = self.require_api()?;
                serde_json::to_value(api.set_type(request)?)
                    .map_err(|error| DbgFlowError::Backend(error.to_string()))
            }
            WorkerRequest::Close { save } => {
                let close_result = if let Some(api) = &self.api {
                    if self.database_open {
                        let result = api.close_database(save);
                        self.database_open = false;
                        result
                    } else {
                        CloseDatabaseResult::no_worker(save)
                    }
                } else {
                    CloseDatabaseResult::no_worker(save)
                };
                self.should_exit = true;
                serde_json::to_value(close_result)
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
        let mut api = DynamicIdaApi::load_and_initialize(&install)?;
        api.open_database(
            &request.target.path().to_string_lossy(),
            request.run_auto_analysis,
        )?;
        let result = OpenIdaDatabaseResult {
            ida: api.info(),
            warnings: rich_api_warnings(&api),
        };
        self.target = Some(request.target);
        self.database_open = true;
        self.api = Some(api);
        serde_json::to_value(result).map_err(|error| DbgFlowError::Backend(error.to_string()))
    }

    fn require_api(&self) -> Result<&DynamicIdaApi> {
        self.api.as_ref().ok_or_else(|| {
            DbgFlowError::Backend("IDA worker does not have an open database".to_string())
        })
    }

    fn require_target(&self) -> Result<&IdaTarget> {
        self.target.as_ref().ok_or_else(|| {
            DbgFlowError::Backend("IDA worker does not have an open target".to_string())
        })
    }
}

fn rich_api_warnings(api: &DynamicIdaApi) -> Vec<String> {
    let status = api.rich_api_status();
    if status.available {
        status.warnings
    } else {
        let detail = if status.missing_symbols.is_empty() {
            "no direct rich API symbols were available".to_string()
        } else {
            format!("missing symbols: {}", status.missing_symbols.join(", "))
        };
        vec![format!("IDA direct rich API unavailable: {detail}")]
    }
}

fn worker_error_from_dbgflow(error: DbgFlowError) -> WorkerErrorInfo {
    match error {
        DbgFlowError::BackendNotFound(message) => {
            WorkerErrorInfo::new("backend_not_found", message)
        }
        DbgFlowError::Backend(message) => WorkerErrorInfo::new("backend", message),
        DbgFlowError::SessionNotFound(session_id) => {
            WorkerErrorInfo::new("session_not_found", session_id.to_string())
        }
        DbgFlowError::SessionClosed(session_id) => {
            WorkerErrorInfo::new("session_closed", session_id.to_string())
        }
        DbgFlowError::Artifact(message) => WorkerErrorInfo::new("artifact", message),
    }
}

fn worker_error_to_dbgflow(error: WorkerErrorInfo) -> DbgFlowError {
    match error.kind.as_str() {
        "backend_not_found" => DbgFlowError::BackendNotFound(error.message),
        "backend" => DbgFlowError::Backend(error.message),
        "artifact" => DbgFlowError::Artifact(error.message),
        "session_not_found" => serde_json::from_value(Value::String(error.message.clone()))
            .map(DbgFlowError::SessionNotFound)
            .unwrap_or_else(|_| {
                DbgFlowError::Backend(format!("session_not_found: {}", error.message))
            }),
        "session_closed" => serde_json::from_value(Value::String(error.message.clone()))
            .map(DbgFlowError::SessionClosed)
            .unwrap_or_else(|_| {
                DbgFlowError::Backend(format!("session_closed: {}", error.message))
            }),
        _ => DbgFlowError::Backend(error.message),
    }
}

fn ida_env_changes(install_dir: &Path) -> Result<Vec<EnvChange>> {
    let mut path_entries = vec![install_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(path_entries).map_err(|error| {
        DbgFlowError::Backend(format!("construct reverse worker PATH: {error}"))
    })?;
    Ok(vec![
        EnvChange::set(IDA_INSTALL_ENV, install_dir.as_os_str()),
        EnvChange::set("PATH", joined),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::io::Cursor;

    #[test]
    fn worker_response_reports_parse_error() {
        let mut out = Vec::new();
        run_reverse_ida_worker_stdio(Cursor::new("{not-json}\n"), &mut out)
            .expect("worker returns parse error");
        let response: WorkerOutput = serde_json::from_slice(&out).expect("worker response");

        let error = response.error.expect("error");
        assert_eq!(error.kind, "backend");
        assert!(error.message.contains("parse worker request"));
        assert!(!error.message.contains("backend error:"));
    }

    #[test]
    fn worker_error_round_trip_does_not_duplicate_backend_prefix() {
        let info = worker_error_from_dbgflow(DbgFlowError::Backend(
            "ida.decompile is unsupported".to_string(),
        ));
        assert_eq!(info.kind, "backend");
        assert_eq!(info.message, "ida.decompile is unsupported");

        let error = worker_error_to_dbgflow(info);
        assert_eq!(
            error.to_string(),
            "backend error: ida.decompile is unsupported"
        );
        assert!(!error.to_string().contains("backend error: backend error:"));
    }

    #[test]
    fn worker_error_round_trip_preserves_session_error_kind() {
        let session_id = SessionId::new();

        let not_found = worker_error_to_dbgflow(worker_error_from_dbgflow(
            DbgFlowError::SessionNotFound(session_id),
        ));
        assert!(matches!(
            not_found,
            DbgFlowError::SessionNotFound(id) if id == session_id
        ));

        let closed = worker_error_to_dbgflow(worker_error_from_dbgflow(
            DbgFlowError::SessionClosed(session_id),
        ));
        assert!(matches!(
            closed,
            DbgFlowError::SessionClosed(id) if id == session_id
        ));
    }

    #[test]
    fn worker_runtime_serializes_errors_without_display_prefix() {
        let mut runtime = WorkerRuntime::default();
        let output = runtime.handle(WorkerInput {
            request_id: 11,
            request: WorkerRequest::Decompile(DecompileRequest {
                target: "0x401000".to_string(),
                include_addresses: true,
            }),
        });

        assert!(!output.ok);
        let error = output.error.expect("error");
        assert_eq!(error.kind, "backend");
        assert!(error.message.contains("open database"));
        assert!(!error.message.contains("backend error:"));
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

    #[test]
    fn reverse_worker_launch_spec_hides_console_window() {
        let executable = PathBuf::from("dbgflow-mcp.exe");
        let install_dir = std::env::temp_dir().join("dbgflow-fake-ida-install");
        let install = IdaInstall {
            ida_dll: install_dir.join("ida.dll"),
            idalib_dll: install_dir.join("idalib.dll"),
            install_dir,
        };
        let worker_log_path = PathBuf::from("worker.log");
        let spec = reverse_worker_launch_spec(&executable, &install, &worker_log_path)
            .expect("build reverse worker launch spec");
        let mut expected = ProcessLaunchSpec::new(&executable);
        expected.hide_console_window();

        assert_eq!(
            spec.args,
            vec![
                OsString::from(REVERSE_WORKER_COMMAND),
                OsString::from(REVERSE_WORKER_KIND_IDA)
            ]
        );
        assert_eq!(spec.stdin, LaunchStdio::Piped);
        assert_eq!(spec.stdout, LaunchStdio::Piped);
        assert_eq!(spec.stderr, LaunchStdio::File(worker_log_path));
        assert_eq!(spec.creation_flags, expected.creation_flags);
    }
}
