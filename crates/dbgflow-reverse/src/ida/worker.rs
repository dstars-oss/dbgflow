use super::install::{
    resolve_supervisor_runtime, IdaRuntimeConfig, IdaSupervisorRuntime, IDA_INSTALL_ENV,
};
use super::model::{IdaOpenMode, IdaUpstreamSession, UpstreamToolDescriptor};
use dbgflow_common::logging::LogSink;
use dbgflow_common::process::{
    log_process_launch, spawn_process, EnvChange, LaunchStdio, ManagedChild, ProcessLaunchContext,
    ProcessLaunchSpec,
};
use dbgflow_common::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::Duration;

const DEFAULT_OPEN_TIMEOUT_MS: u64 = 1_800_000;
const SUPERVISOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

type PendingResponse = std::sync::mpsc::Sender<Result<Value>>;
type PendingResponses = Arc<Mutex<HashMap<u64, PendingResponse>>>;

pub const UPSTREAM_IDB_OPEN: &str = "idb_open";
pub const UPSTREAM_IDB_LIST: &str = "idb_list";
pub const UPSTREAM_IDB_SAVE: &str = "idb_save";

pub const DEFAULT_UPSTREAM_TOOLS: &[&str] = &[
    "add_bookmark",
    "analyze_batch",
    "analyze_component",
    "analyze_function",
    "append_comments",
    "basic_blocks",
    "callees",
    "callgraph",
    "declare_stack",
    "declare_type",
    "decompile",
    "define_code",
    "define_func",
    "delete_stack",
    "diff_before_after",
    "disasm",
    "entity_query",
    "enum_upsert",
    "export_funcs",
    "find",
    "find_bytes",
    "find_regex",
    "find_xref_signatures",
    "force_recompile",
    "func_profile",
    "func_query",
    "get_bytes",
    "get_global_value",
    "get_int",
    "get_string",
    "idb_save",
    "imports",
    "imports_query",
    "infer_types",
    "insn_query",
    "int_convert",
    "list_funcs",
    "list_globals",
    "lookup_funcs",
    "make_data",
    "make_signature",
    "make_signature_for_function",
    "make_signature_for_range",
    "patch",
    "patch_asm",
    "put_int",
    "py_eval",
    "read_struct",
    "rename",
    "search_structs",
    "search_text",
    "server_health",
    "set_comments",
    "set_op_type",
    "set_type",
    "stack_frame",
    "survey_binary",
    "trace_data_flow",
    "type_apply_batch",
    "type_inspect",
    "type_query",
    "undefine",
    "xref_query",
    "xrefs_to",
    "xrefs_to_field",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenIdaDatabase {
    pub input_path: PathBuf,
    pub mode: IdaOpenMode,
    pub run_auto_analysis: bool,
    pub build_caches: bool,
    pub init_hexrays: bool,
    pub idle_ttl_sec: u64,
    pub preferred_session_id: String,
    pub startup_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenIdaDatabaseResult {
    pub session: IdaUpstreamSession,
    pub warmup: Option<Value>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorToolCallResult {
    pub structured_content: Value,
    pub mcp_result: Value,
    pub is_error: bool,
    pub error_message: Option<String>,
}

pub trait IdaSupervisor: Send + Sync {
    fn tool_descriptors(&self) -> Result<Vec<UpstreamToolDescriptor>>;
    fn open_session(&self, request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult>;
    fn list_sessions(&self) -> Result<Vec<IdaUpstreamSession>>;
    fn call_tool(
        &self,
        database_id: &str,
        tool_name: &str,
        arguments: Map<String, Value>,
    ) -> Result<SupervisorToolCallResult>;
    fn has_exited(&self) -> Result<bool>;
    fn kill(&self, reason: &str) -> Result<()>;
}

#[derive(Clone)]
pub struct ProcessIdaSupervisor {
    inner: Arc<ProcessIdaSupervisorInner>,
}

struct ProcessIdaSupervisorInner {
    artifact_root: PathBuf,
    runtime: IdaRuntimeConfig,
    launch_context: ProcessLaunchContext,
    logger: Arc<dyn LogSink>,
    state: Mutex<Option<SupervisorProcess>>,
    active_child: Mutex<Option<Arc<Mutex<ManagedChild>>>>,
    active_pending: Mutex<Option<PendingResponses>>,
}

struct SupervisorProcess {
    child: Arc<Mutex<ManagedChild>>,
    stdin: Arc<Mutex<File>>,
    pending: PendingResponses,
    next_request_id: AtomicU64,
}

impl ProcessIdaSupervisor {
    pub fn new(
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        launch_context: ProcessLaunchContext,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self {
            inner: Arc::new(ProcessIdaSupervisorInner {
                artifact_root: artifact_root.into(),
                runtime,
                launch_context,
                logger,
                state: Mutex::new(None),
                active_child: Mutex::new(None),
                active_pending: Mutex::new(None),
            }),
        }
    }

    fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let method = method.to_string();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params.unwrap_or_else(|| Value::Object(Map::new())),
        });
        self.request_blocking(request, timeout)
    }

    fn request_blocking(&self, mut request: Value, timeout: Option<Duration>) -> Result<Value> {
        let (request_id, stdin, pending) = {
            let mut state = self.inner.state.lock().map_err(|_| {
                DbgFlowError::Backend("IDA supervisor state lock poisoned".to_string())
            })?;
            self.ensure_started_locked(&mut state)?;
            let process = state.as_ref().expect("started above");
            (
                process.next_request_id.fetch_add(1, Ordering::Relaxed),
                process.stdin.clone(),
                process.pending.clone(),
            )
        };
        request["id"] = Value::from(request_id);
        let (tx, rx) = std::sync::mpsc::channel();
        pending
            .lock()
            .map_err(|_| {
                DbgFlowError::Backend("IDA supervisor pending map lock poisoned".to_string())
            })?
            .insert(request_id, tx);

        let write_result = (|| -> Result<()> {
            let mut stdin = stdin.lock().map_err(|_| {
                DbgFlowError::Backend("IDA supervisor stdin lock poisoned".to_string())
            })?;
            serde_json::to_writer(&mut *stdin, &request).map_err(|error| {
                DbgFlowError::Backend(format!("write supervisor request: {error}"))
            })?;
            writeln!(&mut *stdin).map_err(|error| {
                DbgFlowError::Backend(format!("write supervisor request newline: {error}"))
            })?;
            stdin.flush().map_err(|error| {
                DbgFlowError::Backend(format!("flush supervisor request: {error}"))
            })?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = pending
                .lock()
                .map(|mut pending| pending.remove(&request_id));
            return Err(error);
        }

        match timeout {
            Some(timeout) => match rx.recv_timeout(timeout) {
                Ok(result) => result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let _ = pending
                        .lock()
                        .map(|mut pending| pending.remove(&request_id));
                    let _ = self.kill("ida_supervisor_request_timeout");
                    Err(DbgFlowError::Backend(format!(
                        "ida-pro-mcp supervisor request timed out after {} ms",
                        timeout.as_millis()
                    )))
                }
                Err(error) => Err(DbgFlowError::Backend(format!(
                    "ida-pro-mcp supervisor response channel failed: {error}"
                ))),
            },
            None => rx.recv().map_err(|error| {
                DbgFlowError::Backend(format!(
                    "ida-pro-mcp supervisor response channel failed: {error}"
                ))
            })?,
        }
    }

    fn read_supervisor_stdout(mut stdout: BufReader<File>, pending: PendingResponses) {
        loop {
            let mut line = String::new();
            let read = match stdout.read_line(&mut line) {
                Ok(read) => read,
                Err(error) => {
                    fail_pending(
                        &pending,
                        DbgFlowError::Backend(format!("read supervisor response: {error}")),
                    );
                    return;
                }
            };
            if read == 0 {
                fail_pending(
                    &pending,
                    DbgFlowError::Backend(
                        "ida-pro-mcp supervisor exited before responding".to_string(),
                    ),
                );
                return;
            }
            let response: Value = match serde_json::from_str(&line) {
                Ok(response) => response,
                Err(error) => {
                    fail_pending(
                        &pending,
                        DbgFlowError::Backend(format!(
                            "parse supervisor response: {error}: {line}"
                        )),
                    );
                    return;
                }
            };
            let Some(request_id) = response.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let sender = pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&request_id));
            let Some(sender) = sender else {
                continue;
            };
            let result = if let Some(error) = response.get("error") {
                Err(DbgFlowError::Backend(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("ida-pro-mcp supervisor request failed")
                        .to_string(),
                ))
            } else {
                Ok(response.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(result);
        }
    }

    fn ensure_started_locked(&self, state: &mut Option<SupervisorProcess>) -> Result<()> {
        if let Some(process) = state {
            if process
                .child
                .lock()
                .map_err(|_| {
                    DbgFlowError::Backend("IDA supervisor child lock poisoned".to_string())
                })?
                .try_wait()?
                .is_none()
            {
                return Ok(());
            }
            *state = None;
            self.clear_active_child()?;
        }

        let runtime = resolve_supervisor_runtime(&self.inner.runtime)?;
        let profile_path = self.write_default_profile()?;
        let log_path = self.supervisor_log_path()?;
        let spec = supervisor_launch_spec(&runtime, &profile_path, &log_path)?;
        let mut child = spawn_process(&spec, &self.inner.launch_context).map_err(|error| {
            DbgFlowError::Backend(format!(
                "spawn ida-pro-mcp supervisor {} failed: {error}",
                runtime.python_executable.display()
            ))
        })?;
        log_process_launch(
            &self.inner.logger,
            "reverse_ida",
            "ida_supervisor_process_launch_resolved",
            child.audit(),
        );
        let stdin = child.take_stdin().ok_or_else(|| {
            DbgFlowError::Backend("ida-pro-mcp supervisor stdin was not captured".to_string())
        })?;
        let stdout = child.take_stdout().ok_or_else(|| {
            DbgFlowError::Backend("ida-pro-mcp supervisor stdout was not captured".to_string())
        })?;
        let child = Arc::new(Mutex::new(child));
        let stdin = Arc::new(Mutex::new(stdin));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        *self.inner.active_child.lock().map_err(|_| {
            DbgFlowError::Backend("IDA supervisor child handle lock poisoned".to_string())
        })? = Some(child.clone());
        *self.inner.active_pending.lock().map_err(|_| {
            DbgFlowError::Backend("IDA supervisor pending handle lock poisoned".to_string())
        })? = Some(pending.clone());
        let pending_for_reader = pending.clone();
        thread::spawn(move || {
            Self::read_supervisor_stdout(BufReader::new(stdout), pending_for_reader);
        });

        *state = Some(SupervisorProcess {
            child,
            stdin,
            pending,
            next_request_id: AtomicU64::new(1),
        });
        Ok(())
    }

    fn clear_active_child(&self) -> Result<()> {
        *self.inner.active_child.lock().map_err(|_| {
            DbgFlowError::Backend("IDA supervisor child handle lock poisoned".to_string())
        })? = None;
        *self.inner.active_pending.lock().map_err(|_| {
            DbgFlowError::Backend("IDA supervisor pending handle lock poisoned".to_string())
        })? = None;
        Ok(())
    }

    fn write_default_profile(&self) -> Result<PathBuf> {
        let dir = self.inner.artifact_root.join("reverse_sessions");
        fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        let path = dir.join("ida-pro-mcp-dbgflow-profile.txt");
        fs::write(&path, default_profile_text())
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(path)
    }

    fn supervisor_log_path(&self) -> Result<PathBuf> {
        let dir = self.inner.artifact_root.join("reverse_sessions");
        fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        Ok(dir.join("ida-pro-mcp-supervisor.log"))
    }
}

impl IdaSupervisor for ProcessIdaSupervisor {
    fn tool_descriptors(&self) -> Result<Vec<UpstreamToolDescriptor>> {
        let result = self.request("tools/list", None, Some(SUPERVISOR_REQUEST_TIMEOUT))?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                DbgFlowError::Backend("tools/list response missing tools".to_string())
            })?;
        Ok(tools
            .iter()
            .filter_map(upstream_tool_descriptor_from_value)
            .filter(|tool| is_allowed_upstream_tool(&tool.name))
            .collect())
    }

    fn open_session(&self, request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult> {
        let mut arguments = Map::new();
        arguments.insert(
            "input_path".to_string(),
            Value::String(request.input_path.display().to_string()),
        );
        arguments.insert(
            "mode".to_string(),
            Value::String(request.mode.as_upstream().to_string()),
        );
        arguments.insert(
            "run_auto_analysis".to_string(),
            Value::Bool(request.run_auto_analysis),
        );
        arguments.insert(
            "build_caches".to_string(),
            Value::Bool(request.build_caches),
        );
        arguments.insert(
            "init_hexrays".to_string(),
            Value::Bool(request.init_hexrays),
        );
        arguments.insert(
            "idle_ttl_sec".to_string(),
            Value::from(request.idle_ttl_sec),
        );
        arguments.insert(
            "preferred_session_id".to_string(),
            Value::String(request.preferred_session_id),
        );
        arguments.insert(
            "open_timeout_sec".to_string(),
            Value::from((request.startup_timeout_ms.max(1) as f64) / 1000.0),
        );
        let timeout = Duration::from_millis(request.startup_timeout_ms.max(1))
            .saturating_add(Duration::from_secs(30));
        let response = self.call_management_tool(UPSTREAM_IDB_OPEN, arguments, Some(timeout))?;
        if let Some(error) = response
            .structured_content
            .get("error")
            .and_then(Value::as_str)
        {
            return Err(DbgFlowError::Backend(error.to_string()));
        }
        let session = response
            .structured_content
            .get("session")
            .cloned()
            .ok_or_else(|| DbgFlowError::Backend("idb_open response missing session".to_string()))
            .and_then(ida_session_from_value)?;
        Ok(OpenIdaDatabaseResult {
            session,
            warmup: response.structured_content.get("warmup").cloned(),
            message: response
                .structured_content
                .get("message")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        })
    }

    fn list_sessions(&self) -> Result<Vec<IdaUpstreamSession>> {
        let response = self.call_management_tool(UPSTREAM_IDB_LIST, Map::new(), None)?;
        if let Some(error) = response
            .structured_content
            .get("error")
            .and_then(Value::as_str)
        {
            return Err(DbgFlowError::Backend(error.to_string()));
        }
        let sessions = response
            .structured_content
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                DbgFlowError::Backend("idb_list response missing sessions".to_string())
            })?;
        sessions
            .iter()
            .cloned()
            .map(ida_session_from_value)
            .collect()
    }

    fn call_tool(
        &self,
        database_id: &str,
        tool_name: &str,
        mut arguments: Map<String, Value>,
    ) -> Result<SupervisorToolCallResult> {
        arguments.insert(
            "database".to_string(),
            Value::String(database_id.to_string()),
        );
        self.call_management_tool(tool_name, arguments, None)
    }

    fn has_exited(&self) -> Result<bool> {
        let mut state =
            self.inner.state.lock().map_err(|_| {
                DbgFlowError::Backend("IDA supervisor state lock poisoned".to_string())
            })?;
        match state.as_mut() {
            Some(process) => {
                let exited = process
                    .child
                    .lock()
                    .map_err(|_| {
                        DbgFlowError::Backend("IDA supervisor child lock poisoned".to_string())
                    })?
                    .try_wait()?
                    .is_some();
                if exited {
                    *state = None;
                    self.clear_active_child()?;
                }
                Ok(exited)
            }
            None => Ok(true),
        }
    }

    fn kill(&self, _reason: &str) -> Result<()> {
        let child = self
            .inner
            .active_child
            .lock()
            .map_err(|_| {
                DbgFlowError::Backend("IDA supervisor child handle lock poisoned".to_string())
            })?
            .take();
        if let Some(child) = child {
            let mut child = child.lock().map_err(|_| {
                DbgFlowError::Backend("IDA supervisor child lock poisoned".to_string())
            })?;
            if child.try_wait()?.is_none() {
                child.kill()?;
                let _ = child.wait();
            }
        }
        let pending = self
            .inner
            .active_pending
            .lock()
            .map_err(|_| {
                DbgFlowError::Backend("IDA supervisor pending handle lock poisoned".to_string())
            })?
            .take();
        if let Some(pending) = pending {
            fail_pending(
                &pending,
                DbgFlowError::Backend("ida-pro-mcp supervisor was killed".to_string()),
            );
        }
        match self.inner.state.try_lock() {
            Ok(mut state) => *state = None,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Poisoned(_)) => {
                return Err(DbgFlowError::Backend(
                    "IDA supervisor state lock poisoned".to_string(),
                ));
            }
        }
        Ok(())
    }
}

fn fail_pending(pending: &PendingResponses, error: DbgFlowError) {
    let senders = pending
        .lock()
        .map(|mut pending| {
            pending
                .drain()
                .map(|(_, sender)| sender)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let message = error.to_string();
    for sender in senders {
        let _ = sender.send(Err(DbgFlowError::Backend(message.clone())));
    }
}

impl ProcessIdaSupervisor {
    fn call_management_tool(
        &self,
        tool_name: &str,
        arguments: Map<String, Value>,
        timeout: Option<Duration>,
    ) -> Result<SupervisorToolCallResult> {
        let result = self.request(
            "tools/call",
            Some(json!({
                "name": tool_name,
                "arguments": Value::Object(arguments),
            })),
            timeout.or(Some(SUPERVISOR_REQUEST_TIMEOUT)),
        )?;
        decode_tool_result(result)
    }
}

fn supervisor_launch_spec(
    runtime: &IdaSupervisorRuntime,
    profile_path: &Path,
    log_path: &Path,
) -> Result<ProcessLaunchSpec> {
    let mut spec = ProcessLaunchSpec::new(&runtime.python_executable);
    spec.args = vec![
        "-m".into(),
        "ida_pro_mcp.idalib_supervisor".into(),
        "--stdio".into(),
        "--unsafe".into(),
        "--profile".into(),
        profile_path.as_os_str().to_os_string(),
        "--max-workers".into(),
        runtime.max_workers.to_string().into(),
    ];
    spec.env = supervisor_env_changes(runtime)?;
    spec.stdin = LaunchStdio::Piped;
    spec.stdout = LaunchStdio::Piped;
    spec.stderr = LaunchStdio::File(log_path.to_path_buf());
    spec.current_dir = runtime.vendor_src_dir.parent().map(Path::to_path_buf);
    spec.hide_console_window();
    Ok(spec)
}

fn supervisor_env_changes(runtime: &IdaSupervisorRuntime) -> Result<Vec<EnvChange>> {
    let mut python_path_entries = vec![runtime.vendor_src_dir.clone()];
    if let Some(existing) = std::env::var_os("PYTHONPATH") {
        python_path_entries.extend(std::env::split_paths(&existing));
    }
    let python_path = std::env::join_paths(python_path_entries)
        .map_err(|error| DbgFlowError::Backend(format!("construct PYTHONPATH: {error}")))?;

    let mut path_entries = vec![runtime.install.install_dir.clone()];
    if let Some(existing) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(path_entries)
        .map_err(|error| DbgFlowError::Backend(format!("construct PATH: {error}")))?;

    Ok(vec![
        EnvChange::set(IDA_INSTALL_ENV, runtime.install.install_dir.as_os_str()),
        EnvChange::set("PYTHONPATH", python_path),
        EnvChange::set("PATH", path),
        EnvChange::set("PYTHONUNBUFFERED", "1"),
        EnvChange::set(
            "IDA_MCP_OPEN_TIMEOUT",
            (DEFAULT_OPEN_TIMEOUT_MS / 1000).to_string(),
        ),
    ])
}

fn decode_tool_result(result: Value) -> Result<SupervisorToolCallResult> {
    let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
    let structured_content = result
        .get("structuredContent")
        .cloned()
        .unwrap_or_else(|| content_text_json(&result).unwrap_or(Value::Null));
    let error_message = is_error.then(|| tool_error_text(&result));
    Ok(SupervisorToolCallResult {
        structured_content,
        mcp_result: result,
        is_error,
        error_message,
    })
}

fn content_text_json(result: &Value) -> Option<Value> {
    let text = result
        .get("content")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    serde_json::from_str(text).ok()
}

fn tool_error_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("ida-pro-mcp tool call failed")
        .to_string()
}

fn upstream_tool_descriptor_from_value(value: &Value) -> Option<UpstreamToolDescriptor> {
    let name = value.get("name")?.as_str()?.to_string();
    Some(UpstreamToolDescriptor {
        name,
        description: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("IDA Pro MCP upstream tool")
            .to_string(),
        input_schema: value
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(default_upstream_input_schema),
        output_schema: value.get("outputSchema").cloned(),
    })
}

fn ida_session_from_value(value: Value) -> Result<IdaUpstreamSession> {
    let object = value
        .as_object()
        .ok_or_else(|| DbgFlowError::Backend("IDA session entry is not an object".to_string()))?;
    let database_id = object
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if database_id.is_empty() && object.get("adopted").and_then(Value::as_bool) != Some(false) {
        return Err(DbgFlowError::Backend(
            "IDA session entry missing session_id".to_string(),
        ));
    }
    Ok(IdaUpstreamSession {
        database_id,
        input_path: PathBuf::from(
            object
                .get("input_path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        filename: object
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        backend: object
            .get("backend")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        adopted: object.get("adopted").and_then(Value::as_bool),
        owned: object.get("owned").and_then(Value::as_bool),
        pid: object
            .get("pid")
            .and_then(Value::as_u64)
            .map(|pid| pid as u32),
        worker_pid: object
            .get("worker_pid")
            .and_then(Value::as_u64)
            .map(|pid| pid as u32),
        is_active: object.get("is_active").and_then(Value::as_bool),
        is_analyzing: object.get("is_analyzing").and_then(Value::as_bool),
        metadata: object.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

pub fn is_allowed_upstream_tool(tool_name: &str) -> bool {
    DEFAULT_UPSTREAM_TOOLS.contains(&tool_name)
        && tool_name != "py_exec_file"
        && tool_name != UPSTREAM_IDB_OPEN
        && tool_name != UPSTREAM_IDB_LIST
        && !tool_name.starts_with("dbg_")
}

pub fn default_profile_text() -> String {
    let mut text = String::from(
        "# dbgflow default ida-pro-mcp profile\n# Generated by dbgflow; debugger tools and py_exec_file are intentionally excluded.\n\n",
    );
    for tool in DEFAULT_UPSTREAM_TOOLS {
        text.push_str(tool);
        text.push('\n');
    }
    text
}

pub fn fallback_tool_descriptors() -> Vec<UpstreamToolDescriptor> {
    DEFAULT_UPSTREAM_TOOLS
        .iter()
        .map(|tool| UpstreamToolDescriptor {
            name: (*tool).to_string(),
            description: format!("IDA Pro MCP upstream tool `{tool}`."),
            input_schema: default_upstream_input_schema(),
            output_schema: None,
        })
        .collect()
}

fn default_upstream_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "string",
                "description": "dbgflow IDA session id returned by ida.create_session."
            }
        },
        "required": ["session_id"],
        "additionalProperties": true
    })
}

pub fn download_ida_mcp_output(download_url: &str) -> Result<Value> {
    let parsed = parse_http_url(download_url)?;
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port)).map_err(|error| {
        DbgFlowError::Backend(format!(
            "connect to IDA output download endpoint failed: {error}"
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| DbgFlowError::Backend(format!("set read timeout: {error}")))?;
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n\r\n",
        parsed.path, parsed.host_header
    )
    .map_err(|error| DbgFlowError::Backend(format!("write output download request: {error}")))?;
    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut response).map_err(|error| {
        DbgFlowError::Backend(format!("read output download response: {error}"))
    })?;
    let marker = b"\r\n\r\n";
    let body_start = response
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|index| index + marker.len())
        .ok_or_else(|| {
            DbgFlowError::Backend("invalid HTTP response from IDA output endpoint".to_string())
        })?;
    let header = String::from_utf8_lossy(&response[..body_start]);
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        return Err(DbgFlowError::Backend(format!(
            "IDA output download failed: {}",
            header.lines().next().unwrap_or("invalid HTTP status")
        )));
    }
    serde_json::from_slice(&response[body_start..])
        .map_err(|error| DbgFlowError::Backend(format!("parse IDA output JSON: {error}")))
}

struct ParsedHttpUrl {
    host: String,
    host_header: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<ParsedHttpUrl> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        DbgFlowError::Backend("IDA output download URL must use http://".to_string())
    })?;
    let (authority, path) = rest
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
        .ok_or_else(|| DbgFlowError::Backend("IDA output download URL missing port".to_string()))?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err(DbgFlowError::Backend(
            "IDA output download URL must be loopback".to_string(),
        ));
    }
    Ok(ParsedHttpUrl {
        host: host.to_string(),
        host_header: authority.to_string(),
        port,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_includes_py_eval_and_excludes_debugger() {
        let text = default_profile_text();
        assert!(text.lines().any(|line| line == "py_eval"));
        assert!(!text.lines().any(|line| line == "py_exec_file"));
        assert!(!text.lines().any(|line| line.starts_with("dbg_")));
    }

    #[test]
    fn allowed_tool_filter_matches_default_surface() {
        assert!(is_allowed_upstream_tool("decompile"));
        assert!(is_allowed_upstream_tool("py_eval"));
        assert!(!is_allowed_upstream_tool("py_exec_file"));
        assert!(!is_allowed_upstream_tool("dbg_start"));
        assert!(!is_allowed_upstream_tool(UPSTREAM_IDB_OPEN));
    }

    #[test]
    fn parses_loopback_download_url() {
        let parsed = parse_http_url("http://127.0.0.1:1234/output/abc.json").expect("parse");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 1234);
        assert_eq!(parsed.path, "/output/abc.json");
    }
}
