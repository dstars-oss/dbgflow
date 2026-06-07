#[cfg(windows)]
use crate::backend::{dbgeng::DbgEngBackend, DebugBackend};
use crate::backend::{
    BackendEventSink, BackendExecutionEvent, CreateBackendSession, DebugTarget,
    ExecuteBackendRequest, ExecuteBackendResult,
};
use crate::logging::{LogEvent, LogLevel, LogSink};
use crate::proxy::ProxyEnvironment;
use crate::session::SessionId;
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const SESSION_WORKER_COMMAND: &str = "worker";
pub const SESSION_WORKER_KIND_SESSION: &str = "session";

const CLOSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSession {
    pub backend: String,
    pub backend_session_id: String,
    pub warnings: Vec<String>,
}

pub trait SessionWorkerLauncher: Send + Sync {
    fn spawn(
        &self,
        session_id: SessionId,
        logger: Arc<dyn LogSink>,
        proxy: ProxyEnvironment,
    ) -> Result<Arc<dyn SessionWorker>>;
}

pub trait SessionWorker: Send + Sync {
    fn create_session(&self, request: CreateBackendSession) -> Result<WorkerSession>;
    fn execute(
        &self,
        command: String,
        event_sink: Arc<dyn BackendEventSink>,
    ) -> Result<ExecuteBackendResult>;
    fn has_exited(&self) -> Result<bool>;
    fn close(&self) -> Result<()>;
    fn kill(&self, reason: &str) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct ProcessWorkerLauncher {
    executable: Option<PathBuf>,
}

impl ProcessWorkerLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(executable.into()),
        }
    }
}

impl SessionWorkerLauncher for ProcessWorkerLauncher {
    fn spawn(
        &self,
        session_id: SessionId,
        logger: Arc<dyn LogSink>,
        proxy: ProxyEnvironment,
    ) -> Result<Arc<dyn SessionWorker>> {
        let executable = match &self.executable {
            Some(executable) => executable.clone(),
            None => std::env::current_exe().map_err(|error| {
                DbgFlowError::Backend(format!("resolve session worker executable failed: {error}"))
            })?,
        };
        let worker = ProcessSessionWorker::spawn(session_id, executable, logger, proxy)?;
        Ok(Arc::new(worker))
    }
}

#[derive(Clone)]
struct ProcessSessionWorker {
    inner: Arc<ProcessSessionWorkerInner>,
}

struct ProcessSessionWorkerInner {
    session_id: SessionId,
    pid: u32,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<std::io::BufReader<ChildStdout>>,
    request_lock: Mutex<()>,
    next_request_id: AtomicU64,
    logger: Arc<dyn LogSink>,
}

impl Drop for ProcessSessionWorkerInner {
    fn drop(&mut self) {
        let Ok(child) = self.child.get_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(_status)) => {}
            Ok(None) => {
                self.logger.log(
                    LogEvent::new(LogLevel::Warn, "session_worker", "worker_drop_terminate")
                        .session_id(self.session_id)
                        .field("pid", self.pid),
                );
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(error) => {
                self.logger.log(
                    LogEvent::new(LogLevel::Warn, "session_worker", "worker_drop_poll_failed")
                        .session_id(self.session_id)
                        .field("pid", self.pid)
                        .error(error.to_string()),
                );
            }
        }
    }
}

impl ProcessSessionWorker {
    fn spawn(
        session_id: SessionId,
        executable: PathBuf,
        logger: Arc<dyn LogSink>,
        proxy: ProxyEnvironment,
    ) -> Result<Self> {
        let mut command = Command::new(&executable);
        command
            .arg(SESSION_WORKER_COMMAND)
            .arg(SESSION_WORKER_KIND_SESSION)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        apply_proxy_environment(&mut command, &proxy);
        let mut child = command.spawn().map_err(|error| {
            DbgFlowError::Backend(format!(
                "spawn session worker {} failed: {error}",
                executable.display()
            ))
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            DbgFlowError::Backend("session worker stdin was not captured".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            DbgFlowError::Backend("session worker stdout was not captured".to_string())
        })?;

        logger.log(
            LogEvent::new(LogLevel::Info, "session_worker", "worker_spawned")
                .session_id(session_id)
                .field("pid", pid)
                .field("executable", executable.display().to_string())
                .field("proxy_source", format!("{:?}", proxy.source()))
                .field("proxy_keys", proxy.proxy_keys()),
        );

        Ok(Self {
            inner: Arc::new(ProcessSessionWorkerInner {
                session_id,
                pid,
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(std::io::BufReader::new(stdout)),
                request_lock: Mutex::new(()),
                next_request_id: AtomicU64::new(0),
                logger,
            }),
        })
    }

    fn request(&self, request: WorkerRequest) -> Result<WorkerResult> {
        self.request_with_event_sink(request, None)
    }

    fn request_with_event_sink(
        &self,
        request: WorkerRequest,
        event_sink: Option<Arc<dyn BackendEventSink>>,
    ) -> Result<WorkerResult> {
        let _request_guard = self
            .inner
            .request_lock
            .lock()
            .map_err(|_| DbgFlowError::Backend("worker request lock poisoned".to_string()))?;
        if self.has_exited()? {
            return Err(DbgFlowError::Backend(format!(
                "session worker process already exited: pid {}",
                self.inner.pid
            )));
        }

        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let input = WorkerInput {
            request_id,
            request,
        };
        let method = input.request.method_name().to_string();
        let request_started = Instant::now();
        self.inner.logger.log(
            LogEvent::new(LogLevel::Info, "session_worker", "worker_request_started")
                .session_id(self.inner.session_id)
                .field("pid", self.inner.pid)
                .field("request_id", request_id)
                .field("method", method.clone()),
        );
        {
            let mut stdin = self
                .inner
                .stdin
                .lock()
                .map_err(|_| DbgFlowError::Backend("worker stdin lock poisoned".to_string()))?;
            if let Err(error) = serde_json::to_writer(&mut *stdin, &input) {
                let error = DbgFlowError::Backend(format!("write worker request failed: {error}"));
                self.log_worker_request_failed(request_id, &method, request_started, &error);
                return Err(error);
            }
            if let Err(error) = writeln!(&mut *stdin) {
                let error =
                    DbgFlowError::Backend(format!("write worker request newline failed: {error}"));
                self.log_worker_request_failed(request_id, &method, request_started, &error);
                return Err(error);
            }
            if let Err(error) = stdin.flush() {
                let error = DbgFlowError::Backend(format!("flush worker request failed: {error}"));
                self.log_worker_request_failed(request_id, &method, request_started, &error);
                return Err(error);
            }
        }

        loop {
            let mut line = String::new();
            let read = match self
                .inner
                .stdout
                .lock()
                .map_err(|_| DbgFlowError::Backend("worker stdout lock poisoned".to_string()))?
                .read_line(&mut line)
            {
                Ok(read) => read,
                Err(error) => {
                    let error =
                        DbgFlowError::Backend(format!("read worker response failed: {error}"));
                    self.log_worker_request_failed(request_id, &method, request_started, &error);
                    return Err(error);
                }
            };
            if read == 0 {
                let error = DbgFlowError::Backend(format!(
                    "session worker process exited before response: pid {}",
                    self.inner.pid
                ));
                self.log_worker_request_failed(request_id, &method, request_started, &error);
                return Err(error);
            }

            let output = match serde_json::from_str::<WorkerOutput>(&line) {
                Ok(output) => output,
                Err(error) => {
                    let error =
                        DbgFlowError::Backend(format!("parse worker response failed: {error}"));
                    self.log_worker_request_failed(request_id, &method, request_started, &error);
                    return Err(error);
                }
            };
            match output {
                WorkerOutput::Log { event } => self.inner.logger.log(event),
                WorkerOutput::ExecutionStateChanged { event } => {
                    if let Some(event_sink) = &event_sink {
                        event_sink.execution_state_changed(event);
                    }
                }
                WorkerOutput::Response {
                    request_id: response_id,
                    result,
                    error,
                } => {
                    if response_id != request_id {
                        let error = DbgFlowError::Backend(format!(
                            "unexpected worker response id: expected {request_id}, got {response_id}"
                        ));
                        self.log_worker_request_failed(
                            request_id,
                            &method,
                            request_started,
                            &error,
                        );
                        return Err(error);
                    }
                    if let Some(error) = error {
                        let error = DbgFlowError::Backend(error);
                        self.log_worker_request_failed(
                            request_id,
                            &method,
                            request_started,
                            &error,
                        );
                        return Err(error);
                    }
                    let result = result.ok_or_else(|| {
                        DbgFlowError::Backend("worker response missing result".to_string())
                    });
                    match &result {
                        Ok(result) => self.inner.logger.log(
                            LogEvent::new(
                                LogLevel::Info,
                                "session_worker",
                                "worker_request_finished",
                            )
                            .session_id(self.inner.session_id)
                            .duration_ms(request_started.elapsed().as_millis())
                            .field("pid", self.inner.pid)
                            .field("request_id", request_id)
                            .field("method", method.clone())
                            .field("result", result.result_name()),
                        ),
                        Err(error) => self.log_worker_request_failed(
                            request_id,
                            &method,
                            request_started,
                            error,
                        ),
                    }
                    return result;
                }
            }
        }
    }

    fn log_worker_request_failed(
        &self,
        request_id: u64,
        method: &str,
        started: Instant,
        error: &DbgFlowError,
    ) {
        self.inner.logger.log(
            LogEvent::new(LogLevel::Error, "session_worker", "worker_request_failed")
                .session_id(self.inner.session_id)
                .duration_ms(started.elapsed().as_millis())
                .field("pid", self.inner.pid)
                .field("request_id", request_id)
                .field("method", method)
                .error(error.to_string()),
        );
    }

    fn request_with_timeout(
        &self,
        request: WorkerRequest,
        timeout: Duration,
    ) -> Result<WorkerResult> {
        let method = request.method_name().to_string();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = self.clone();
        thread::spawn(move || {
            let _ = tx.send(worker.request(request));
        });

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.inner.logger.log(
                    LogEvent::new(LogLevel::Error, "session_worker", "worker_request_timeout")
                        .session_id(self.inner.session_id)
                        .field("pid", self.inner.pid)
                        .field("method", method)
                        .field("timeout_ms", timeout.as_millis() as u64),
                );
                self.kill("worker_request_timeout")?;
                Err(DbgFlowError::Backend(format!(
                    "session worker request timed out after {} ms",
                    timeout.as_millis()
                )))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(DbgFlowError::Backend(
                "session worker request thread exited without a response".to_string(),
            )),
        }
    }

    fn has_exited(&self) -> Result<bool> {
        let mut child = self
            .inner
            .child
            .lock()
            .map_err(|_| DbgFlowError::Backend("worker child lock poisoned".to_string()))?;
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| DbgFlowError::Backend(format!("poll worker failed: {error}")))
    }

    fn wait_for_exit_or_kill(&self, reason: &str, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        loop {
            if self.has_exited()? {
                self.inner.logger.log(
                    LogEvent::new(LogLevel::Info, "session_worker", "worker_exited")
                        .session_id(self.inner.session_id)
                        .duration_ms(started.elapsed().as_millis())
                        .field("pid", self.inner.pid)
                        .field("reason", reason),
                );
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return self.kill(reason);
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

fn apply_proxy_environment(command: &mut Command, proxy: &ProxyEnvironment) {
    for key in proxy.removed_keys() {
        command.env_remove(key);
    }
    for (key, value) in proxy.env_vars() {
        command.env(key, value);
    }
}

impl SessionWorker for ProcessSessionWorker {
    fn create_session(&self, request: CreateBackendSession) -> Result<WorkerSession> {
        match self.request(WorkerRequest::CreateSession {
            target: request.target,
            correlation_id: request.correlation_id,
        })? {
            WorkerResult::SessionCreated {
                backend,
                backend_session_id,
                warnings,
            } => Ok(WorkerSession {
                backend,
                backend_session_id,
                warnings,
            }),
            other => Err(DbgFlowError::Backend(format!(
                "unexpected worker create response: {other:?}"
            ))),
        }
    }

    fn execute(
        &self,
        command: String,
        event_sink: Arc<dyn BackendEventSink>,
    ) -> Result<ExecuteBackendResult> {
        match self.request_with_event_sink(WorkerRequest::Execute { command }, Some(event_sink))? {
            WorkerResult::Executed {
                output,
                warnings,
                final_state,
            } => Ok(ExecuteBackendResult {
                output,
                warnings,
                final_state,
            }),
            other => Err(DbgFlowError::Backend(format!(
                "unexpected worker execute response: {other:?}"
            ))),
        }
    }

    fn close(&self) -> Result<()> {
        if ProcessSessionWorker::has_exited(self)? {
            return Ok(());
        }
        match self.request_with_timeout(WorkerRequest::CloseSession, CLOSE_REQUEST_TIMEOUT) {
            Ok(WorkerResult::Closed) => {
                self.wait_for_exit_or_kill("close_session", EXIT_WAIT_TIMEOUT)
            }
            Ok(other) => {
                self.kill("unexpected_close_response")?;
                Err(DbgFlowError::Backend(format!(
                    "unexpected worker close response: {other:?}"
                )))
            }
            Err(error) => {
                let _ = self.kill("close_session_failed");
                Err(error)
            }
        }
    }

    fn has_exited(&self) -> Result<bool> {
        ProcessSessionWorker::has_exited(self)
    }

    fn kill(&self, reason: &str) -> Result<()> {
        let started = Instant::now();
        let mut child = self
            .inner
            .child
            .lock()
            .map_err(|_| DbgFlowError::Backend("worker child lock poisoned".to_string()))?;
        if let Some(status) = child
            .try_wait()
            .map_err(|error| DbgFlowError::Backend(format!("poll worker failed: {error}")))?
        {
            self.inner.logger.log(
                LogEvent::new(LogLevel::Info, "session_worker", "worker_already_exited")
                    .session_id(self.inner.session_id)
                    .duration_ms(started.elapsed().as_millis())
                    .field("pid", self.inner.pid)
                    .field("reason", reason)
                    .field("status", status.to_string()),
            );
            return Ok(());
        }

        self.inner.logger.log(
            LogEvent::new(
                LogLevel::Warn,
                "session_worker",
                "worker_terminate_requested",
            )
            .session_id(self.inner.session_id)
            .field("pid", self.inner.pid)
            .field("reason", reason),
        );
        child
            .kill()
            .map_err(|error| DbgFlowError::Backend(format!("terminate worker failed: {error}")))?;
        let status = child
            .wait()
            .map_err(|error| DbgFlowError::Backend(format!("wait worker failed: {error}")))?;
        self.inner.logger.log(
            LogEvent::new(LogLevel::Info, "session_worker", "worker_terminated")
                .session_id(self.inner.session_id)
                .duration_ms(started.elapsed().as_millis())
                .field("pid", self.inner.pid)
                .field("reason", reason)
                .field("status", status.to_string()),
        );
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerInput {
    request_id: u64,
    request: WorkerRequest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum WorkerRequest {
    CreateSession {
        target: DebugTarget,
        correlation_id: Option<String>,
    },
    Execute {
        command: String,
    },
    CloseSession,
}

impl WorkerRequest {
    fn method_name(&self) -> &'static str {
        match self {
            Self::CreateSession { .. } => "create_session",
            Self::Execute { .. } => "execute",
            Self::CloseSession => "close_session",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerOutput {
    Response {
        request_id: u64,
        result: Option<WorkerResult>,
        error: Option<String>,
    },
    Log {
        event: LogEvent,
    },
    ExecutionStateChanged {
        event: BackendExecutionEvent,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum WorkerResult {
    SessionCreated {
        backend: String,
        backend_session_id: String,
        warnings: Vec<String>,
    },
    Executed {
        output: String,
        warnings: Vec<String>,
        final_state: Option<crate::backend::BackendExecutionState>,
    },
    Closed,
}

impl WorkerResult {
    fn result_name(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session_created",
            Self::Executed { .. } => "executed",
            Self::Closed => "closed",
        }
    }
}

pub fn run_session_worker_stdio<R, W>(input: R, output: W) -> std::io::Result<()>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    let output = Arc::new(Mutex::new(output));
    let logger: Arc<dyn LogSink> = Arc::new(WorkerLogSink {
        output: output.clone(),
    });

    #[cfg(windows)]
    let mut runtime = WorkerRuntime::new(logger);
    #[cfg(not(windows))]
    let mut runtime = WorkerRuntime::new();

    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let envelope = match serde_json::from_str::<WorkerInput>(&line) {
            Ok(envelope) => envelope,
            Err(error) => {
                write_response(
                    &output,
                    0,
                    Err(DbgFlowError::Backend(format!(
                        "invalid session worker request: {error}"
                    ))),
                )?;
                continue;
            }
        };

        let (result, should_exit) =
            runtime.handle(envelope.request, envelope.request_id, output.clone());
        write_response(&output, envelope.request_id, result)?;
        if should_exit {
            break;
        }
    }

    Ok(())
}

fn write_response<W: Write>(
    output: &Arc<Mutex<W>>,
    request_id: u64,
    result: Result<WorkerResult>,
) -> std::io::Result<()> {
    let frame = match result {
        Ok(result) => WorkerOutput::Response {
            request_id,
            result: Some(result),
            error: None,
        },
        Err(error) => WorkerOutput::Response {
            request_id,
            result: None,
            error: Some(error.to_string()),
        },
    };
    write_worker_output(output, &frame)
}

fn write_worker_output<W: Write>(
    output: &Arc<Mutex<W>>,
    frame: &WorkerOutput,
) -> std::io::Result<()> {
    let mut output = output.lock().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "worker output lock poisoned")
    })?;
    serde_json::to_writer(&mut *output, frame)?;
    writeln!(&mut *output)?;
    output.flush()
}

struct WorkerLogSink<W: Write + Send + 'static> {
    output: Arc<Mutex<W>>,
}

impl<W: Write + Send + 'static> LogSink for WorkerLogSink<W> {
    fn log(&self, event: LogEvent) {
        let _ = write_worker_output(&self.output, &WorkerOutput::Log { event });
    }
}

#[cfg(windows)]
struct WorkerRuntime {
    backend: DbgEngBackend,
    backend_session_id: Option<String>,
}

#[cfg(windows)]
impl WorkerRuntime {
    fn new(logger: Arc<dyn LogSink>) -> Self {
        Self {
            backend: DbgEngBackend::with_logger(logger),
            backend_session_id: None,
        }
    }

    fn handle<W: Write + Send + 'static>(
        &mut self,
        request: WorkerRequest,
        _request_id: u64,
        output: Arc<Mutex<W>>,
    ) -> (Result<WorkerResult>, bool) {
        match request {
            WorkerRequest::CreateSession {
                target,
                correlation_id,
            } => {
                if self.backend_session_id.is_some() {
                    return (
                        Err(DbgFlowError::Backend(
                            "worker backend session already exists".to_string(),
                        )),
                        true,
                    );
                }
                match self.backend.create_session(CreateBackendSession {
                    target,
                    correlation_id,
                }) {
                    Ok(session) => {
                        self.backend_session_id = Some(session.id.clone());
                        (
                            Ok(WorkerResult::SessionCreated {
                                backend: self.backend.info().name,
                                backend_session_id: session.id,
                                warnings: session.warnings,
                            }),
                            false,
                        )
                    }
                    Err(error) => (Err(error), true),
                }
            }
            WorkerRequest::Execute { command } => {
                let Some(backend_session_id) = self.backend_session_id.clone() else {
                    return (
                        Err(DbgFlowError::Backend(
                            "worker backend session is not initialized".to_string(),
                        )),
                        true,
                    );
                };
                (
                    self.backend
                        .execute(
                            ExecuteBackendRequest {
                                backend_session_id,
                                command,
                            },
                            Arc::new(WorkerProtocolEventSink { output }),
                        )
                        .map(|result| WorkerResult::Executed {
                            output: result.output,
                            warnings: result.warnings,
                            final_state: result.final_state,
                        }),
                    false,
                )
            }
            WorkerRequest::CloseSession => {
                let Some(backend_session_id) = self.backend_session_id.take() else {
                    return (Ok(WorkerResult::Closed), true);
                };
                (
                    self.backend
                        .close_session(&backend_session_id)
                        .map(|_| WorkerResult::Closed),
                    true,
                )
            }
        }
    }
}

struct WorkerProtocolEventSink<W: Write + Send + 'static> {
    output: Arc<Mutex<W>>,
}

impl<W: Write + Send + 'static> BackendEventSink for WorkerProtocolEventSink<W> {
    fn execution_state_changed(&self, event: BackendExecutionEvent) {
        let _ = write_worker_output(&self.output, &WorkerOutput::ExecutionStateChanged { event });
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_proxy_environment, run_session_worker_stdio};
    use crate::proxy::ProxyEnvironment;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::io::{Cursor, Write};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    #[test]
    fn worker_stdio_handles_close_session_request() {
        let output = Arc::new(Mutex::new(Vec::new()));
        run_session_worker_stdio(
            Cursor::new(r#"{"request_id":1,"request":{"method":"close_session"}}"#),
            SharedWriter(output.clone()),
        )
        .expect("run worker stdio");

        let output =
            String::from_utf8(output.lock().expect("output lock").clone()).expect("utf8 output");
        let response: Value = serde_json::from_str(output.trim()).expect("worker response");
        assert_eq!(response["kind"], "response");
        assert_eq!(response["request_id"], 1);
        assert_eq!(response["result"]["result"], "closed");
        assert_eq!(response["error"], Value::Null);
    }

    #[test]
    fn apply_proxy_environment_sets_cli_proxy_vars_and_removes_unused_proxy_keys() {
        let mut command = Command::new("dbgflow-test");
        let proxy =
            ProxyEnvironment::from_cli_proxy_url("http://127.0.0.1:7897").expect("parse proxy");

        apply_proxy_environment(&mut command, &proxy);

        let envs = command_envs(&command);
        assert_eq!(env_value(&envs, "_NT_SYMBOL_PROXY"), Some("127.0.0.1:7897"));
        assert_eq!(
            env_value(&envs, "HTTP_PROXY"),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            env_value(&envs, "HTTPS_PROXY"),
            Some("http://127.0.0.1:7897")
        );
        assert!(env_removed(&envs, "ALL_PROXY"));
        assert!(env_removed(&envs, "NO_PROXY"));
    }

    #[test]
    fn apply_proxy_environment_leaves_env_unchanged_when_proxy_is_none() {
        let mut command = Command::new("dbgflow-test");

        apply_proxy_environment(&mut command, &ProxyEnvironment::none());

        let envs = command_envs(&command);
        for key in [
            "_NT_SYMBOL_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            assert!(!envs.contains_key(key), "expected {key} to be untouched");
        }
    }

    #[test]
    fn apply_proxy_environment_removes_known_proxy_keys_when_disabled() {
        let mut command = Command::new("dbgflow-test");

        apply_proxy_environment(&mut command, &ProxyEnvironment::disabled());

        let envs = command_envs(&command);
        for key in [
            "_NT_SYMBOL_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
        ] {
            assert!(env_removed(&envs, key), "expected {key} to be removed");
        }
        for key in ["http_proxy", "https_proxy", "all_proxy", "no_proxy"] {
            assert!(
                env_removed(&envs, key) || (cfg!(windows) && env_value(&envs, key).is_none()),
                "expected {key} to be removed"
            );
        }
    }

    fn command_envs(command: &Command) -> HashMap<String, Option<String>> {
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn env_value<'a>(envs: &'a HashMap<String, Option<String>>, key: &str) -> Option<&'a str> {
        envs.get(key).and_then(|value| value.as_deref())
    }

    fn env_removed(envs: &HashMap<String, Option<String>>, key: &str) -> bool {
        matches!(envs.get(key), Some(None))
    }

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("writer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
struct WorkerRuntime;

#[cfg(not(windows))]
impl WorkerRuntime {
    fn new() -> Self {
        Self
    }

    fn handle<W: Write + Send + 'static>(
        &mut self,
        request: WorkerRequest,
        _request_id: u64,
        _output: Arc<Mutex<W>>,
    ) -> (Result<WorkerResult>, bool) {
        match request {
            WorkerRequest::CreateSession { .. } => (
                Err(DbgFlowError::Backend(
                    "real debug sessions are only supported on Windows".to_string(),
                )),
                true,
            ),
            WorkerRequest::Execute { .. } => (
                Err(DbgFlowError::Backend(
                    "worker backend session is not initialized".to_string(),
                )),
                true,
            ),
            WorkerRequest::CloseSession => (Ok(WorkerResult::Closed), true),
        }
    }
}
