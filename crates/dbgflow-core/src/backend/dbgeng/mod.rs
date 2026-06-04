use super::{
    BackendCapability, BackendInfo, BackendKind, BackendSession, CreateBackendSession,
    DebugBackend, DebugTarget, ExecuteBackendRequest, ExecuteBackendResult,
};
use crate::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use crate::{DbgFlowError, Result};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{implement, Interface, Type, GUID, HRESULT, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::Debug::DebugBreakProcess;
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    IDebugClient, IDebugClient4, IDebugClient5, IDebugControl4, IDebugOutputCallbacksWide,
    IDebugOutputCallbacksWide_Impl, DEBUG_ATTACH_DEFAULT, DEBUG_END_PASSIVE, DEBUG_EXECUTE_DEFAULT,
    DEBUG_OUTCTL_ALL_CLIENTS, DEBUG_STATUS_GO,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};
use windows::Win32::System::Threading::{
    CreateProcessW, OpenProcess, ResumeThread, TerminateProcess, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, PROCESS_CREATE_THREAD, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_WRITE, STARTUPINFOW,
};

mod resolver;

pub use resolver::{resolve_dbgeng, DbgEngLocation, DbgEngSource};

const DEFAULT_WAIT_TIMEOUT_MS: u32 = 120_000;
const S_OK: HRESULT = HRESULT(0);
const S_FALSE: HRESULT = HRESULT(1);
const E_UNEXPECTED: HRESULT = HRESULT(0x8000FFFFu32 as i32);

pub struct DbgEngBackend {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, WorkerHandle>>,
    logger: Arc<dyn LogSink>,
}

impl DbgEngBackend {
    pub fn new() -> Self {
        Self::with_logger(noop_logger())
    }

    pub fn with_logger(logger: Arc<dyn LogSink>) -> Self {
        Self {
            next_id: AtomicU64::new(0),
            sessions: Mutex::new(HashMap::new()),
            logger,
        }
    }
}

impl Default for DbgEngBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugBackend for DbgEngBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "dbgeng".to_string(),
            kind: BackendKind::DbgEng,
            capabilities: vec![
                BackendCapability::SessionLifecycle,
                BackendCapability::Execute,
                BackendCapability::DumpAnalysis,
                BackendCapability::LaunchProcess,
                BackendCapability::AttachProcess,
            ],
        }
    }

    fn create_session(&self, request: CreateBackendSession) -> Result<BackendSession> {
        if matches!(request.target, DebugTarget::Mock) {
            return Err(DbgFlowError::Backend(
                "DbgEngBackend does not support mock targets".to_string(),
            ));
        }

        let id = format!("dbgeng-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let worker_id = id.clone();
        let logger = self.logger.clone();
        let correlation_id = request.correlation_id.clone();
        let join = thread::spawn(move || {
            worker_main(
                worker_id,
                request.target,
                correlation_id,
                logger,
                rx,
                init_tx,
            )
        });

        let warnings = init_rx.recv().map_err(|_| {
            DbgFlowError::Backend("dbgeng worker exited during startup".to_string())
        })??;

        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?
            .insert(
                id.clone(),
                WorkerHandle {
                    tx,
                    join: Some(join),
                },
            );

        Ok(BackendSession { id, warnings })
    }

    fn execute(&self, request: ExecuteBackendRequest) -> Result<ExecuteBackendResult> {
        let tx = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?
            .get(&request.backend_session_id)
            .map(|handle| handle.tx.clone())
            .ok_or_else(|| {
                DbgFlowError::Backend(format!(
                    "dbgeng session not found: {}",
                    request.backend_session_id
                ))
            })?;

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        tx.send(WorkerCommand::Execute {
            command: request.command,
            timeout_ms: request.timeout_ms,
            reply: reply_tx,
        })
        .map_err(|_| DbgFlowError::Backend("dbgeng worker is not available".to_string()))?;

        reply_rx
            .recv_timeout(reply_timeout(request.timeout_ms))
            .map_err(|_| DbgFlowError::Backend("dbgeng execute timed out".to_string()))?
    }

    fn close_session(&self, backend_session_id: &str) -> Result<()> {
        let tx = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?
            .get(backend_session_id)
            .map(|handle| handle.tx.clone())
            .ok_or_else(|| {
                DbgFlowError::Backend(format!("dbgeng session not found: {backend_session_id}"))
            })?;

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if tx.send(WorkerCommand::Close { reply: reply_tx }).is_err() {
            self.logger.log(
                LogEvent::new(LogLevel::Error, "dbgeng", "close_worker_unavailable")
                    .backend_session_id(backend_session_id.to_string())
                    .error("dbgeng worker is not available"),
            );
            let mut handle = self.remove_worker_handle(backend_session_id)?;
            if let Some(join) = handle.join.take() {
                let _ = join.join();
            }
            return Err(DbgFlowError::Backend(
                "dbgeng worker is not available".to_string(),
            ));
        }

        let result = match reply_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => result,
            Err(_) => {
                self.logger.log(
                    LogEvent::new(LogLevel::Error, "dbgeng", "close_timed_out")
                        .backend_session_id(backend_session_id.to_string())
                        .error("dbgeng close timed out"),
                );
                return Err(DbgFlowError::Backend("dbgeng close timed out".to_string()));
            }
        };

        let mut handle = self.remove_worker_handle(backend_session_id)?;

        if let Some(join) = handle.join.take() {
            let _ = join.join();
        }

        result
    }
}

impl DbgEngBackend {
    fn remove_worker_handle(&self, backend_session_id: &str) -> Result<WorkerHandle> {
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?
            .remove(backend_session_id)
            .ok_or_else(|| {
                DbgFlowError::Backend(format!("dbgeng session not found: {backend_session_id}"))
            })
    }
}

struct WorkerHandle {
    tx: mpsc::Sender<WorkerCommand>,
    join: Option<thread::JoinHandle<()>>,
}

enum WorkerCommand {
    Execute {
        command: String,
        timeout_ms: u64,
        reply: mpsc::SyncSender<Result<ExecuteBackendResult>>,
    },
    Close {
        reply: mpsc::SyncSender<Result<()>>,
    },
}

fn worker_main(
    id: String,
    target: DebugTarget,
    correlation_id: Option<String>,
    logger: Arc<dyn LogSink>,
    rx: mpsc::Receiver<WorkerCommand>,
    init_tx: mpsc::SyncSender<Result<Vec<String>>>,
) {
    let mut session = match DbgEngSession::open_target(&id, correlation_id, &target, logger) {
        Ok(session) => {
            let _ = init_tx.send(Ok(session.warnings.clone()));
            session
        }
        Err(error) => {
            let _ = init_tx.send(Err(error));
            return;
        }
    };

    while let Ok(command) = rx.recv() {
        match command {
            WorkerCommand::Execute {
                command,
                timeout_ms,
                reply,
            } => {
                let _ = reply.send(session.execute(&command, timeout_ms));
            }
            WorkerCommand::Close { reply } => {
                let result = session.close();
                let _ = reply.send(result);
                break;
            }
        }
    }
}

struct DbgEngSession {
    _library: LoadedDbgEng,
    client: IDebugClient,
    _client4: IDebugClient4,
    client5: IDebugClient5,
    control: IDebugControl4,
    _callbacks: IDebugOutputCallbacksWide,
    output: Arc<Mutex<String>>,
    _launched_process: Option<LaunchedProcess>,
    warnings: Vec<String>,
    logger: Arc<dyn LogSink>,
    correlation_id: Option<String>,
    backend_session_id: String,
}

impl DbgEngSession {
    fn open_target(
        backend_session_id: &str,
        correlation_id: Option<String>,
        target: &DebugTarget,
        logger: Arc<dyn LogSink>,
    ) -> Result<Self> {
        let startup_started = Instant::now();
        logger.log(
            dbgeng_event(
                LogLevel::Info,
                "open_target_started",
                &correlation_id,
                Some(backend_session_id),
            )
            .field("target", target),
        );

        let _dbgeng_guard = global_dbgeng_lock().lock().map_err(|_| {
            DbgFlowError::Backend("global dbgeng operation lock poisoned".to_string())
        })?;

        let resolve_started = Instant::now();
        let location = resolve_dbgeng()?;
        logger.log(
            dbgeng_event(
                LogLevel::Info,
                "resolve_dbgeng_finished",
                &correlation_id,
                Some(backend_session_id),
            )
            .duration_ms(resolve_started.elapsed().as_millis())
            .field("source", &location.source)
            .field("path", location.path.display().to_string()),
        );
        let mut warnings = Vec::new();
        warnings.push(format!(
            "loaded dbgeng.dll from {:?}: {}",
            location.source,
            location.path.display()
        ));
        if matches!(location.source, DbgEngSource::System32) {
            warnings.push("System32 dbgeng.dll is the last-resort fallback".to_string());
        }

        let library_started = Instant::now();
        let library = LoadedDbgEng::load(location.path)?;
        logger.log(
            dbgeng_event(
                LogLevel::Info,
                "load_library_finished",
                &correlation_id,
                Some(backend_session_id),
            )
            .duration_ms(library_started.elapsed().as_millis()),
        );
        let debug_create_started = Instant::now();
        let client = library.debug_create::<IDebugClient>()?;
        let client4: IDebugClient4 = client.cast().map_err(|error| {
            DbgFlowError::Backend(format!("query IDebugClient4 failed: {error}"))
        })?;
        let client5: IDebugClient5 = client.cast().map_err(|error| {
            DbgFlowError::Backend(format!("query IDebugClient5 failed: {error}"))
        })?;
        let control: IDebugControl4 = client.cast().map_err(|error| {
            DbgFlowError::Backend(format!("query IDebugControl4 failed: {error}"))
        })?;
        logger.log(
            dbgeng_event(
                LogLevel::Info,
                "debug_create_finished",
                &correlation_id,
                Some(backend_session_id),
            )
            .duration_ms(debug_create_started.elapsed().as_millis()),
        );

        let output = Arc::new(Mutex::new(String::new()));
        let callbacks: IDebugOutputCallbacksWide = DbgEngOutputCallbacks {
            output: Arc::clone(&output),
        }
        .into();

        let mut launched_process = None;

        unsafe {
            let callback_started = Instant::now();
            client5
                .SetOutputCallbacksWide(&callbacks)
                .map_err(|error| {
                    DbgFlowError::Backend(format!("SetOutputCallbacksWide failed: {error}"))
                })?;
            logger.log(
                dbgeng_event(
                    LogLevel::Info,
                    "set_output_callbacks_finished",
                    &correlation_id,
                    Some(backend_session_id),
                )
                .duration_ms(callback_started.elapsed().as_millis()),
            );

            match target {
                DebugTarget::Dump { path } => {
                    let wide_path = to_wide_null(path);
                    let open_started = Instant::now();
                    client4
                        .OpenDumpFileWide(PCWSTR(wide_path.as_ptr()), 0)
                        .map_err(|error| {
                            logger.log(
                                dbgeng_event(
                                    LogLevel::Error,
                                    "open_dump_file_failed",
                                    &correlation_id,
                                    Some(backend_session_id),
                                )
                                .duration_ms(open_started.elapsed().as_millis())
                                .error(error.to_string())
                                .field("path", path.display().to_string()),
                            );
                            DbgFlowError::Backend(format!("OpenDumpFileWide failed: {error}"))
                        })?;
                    logger.log(
                        dbgeng_event(
                            LogLevel::Info,
                            "open_dump_file_finished",
                            &correlation_id,
                            Some(backend_session_id),
                        )
                        .duration_ms(open_started.elapsed().as_millis())
                        .field("path", path.display().to_string()),
                    );
                    let wait_started = Instant::now();
                    control
                        .WaitForEvent(0, DEFAULT_WAIT_TIMEOUT_MS)
                        .map_err(|error| {
                            logger.log(
                                dbgeng_event(
                                    LogLevel::Error,
                                    "wait_for_event_failed",
                                    &correlation_id,
                                    Some(backend_session_id),
                                )
                                .duration_ms(wait_started.elapsed().as_millis())
                                .error(error.to_string())
                                .field("timeout_ms", DEFAULT_WAIT_TIMEOUT_MS),
                            );
                            DbgFlowError::Backend(format!("WaitForEvent failed: {error}"))
                        })?;
                    logger.log(
                        dbgeng_event(
                            LogLevel::Info,
                            "wait_for_event_finished",
                            &correlation_id,
                            Some(backend_session_id),
                        )
                        .duration_ms(wait_started.elapsed().as_millis())
                        .field("timeout_ms", DEFAULT_WAIT_TIMEOUT_MS),
                    );
                }
                DebugTarget::Attach { pid } => {
                    attach_process(&client4, *pid)?;
                    break_process(*pid)?;
                    control
                        .WaitForEvent(0, DEFAULT_WAIT_TIMEOUT_MS)
                        .map_err(|error| {
                            DbgFlowError::Backend(format!(
                                "WaitForEvent after attach failed: {error}"
                            ))
                        })?;
                }
                DebugTarget::Launch { executable, args } => {
                    let mut launched = create_suspended_process(executable, args)?;
                    attach_process(&client4, launched.pid)?;
                    launched.resume()?;
                    control
                        .WaitForEvent(0, DEFAULT_WAIT_TIMEOUT_MS)
                        .map_err(|error| {
                            DbgFlowError::Backend(format!(
                                "WaitForEvent after launch failed: {error}"
                            ))
                        })?;
                    launched_process = Some(launched);
                }
                DebugTarget::Mock => {
                    return Err(DbgFlowError::Backend(
                        "DbgEngSession does not support mock targets".to_string(),
                    ));
                }
            }
        }

        logger.log(
            dbgeng_event(
                LogLevel::Info,
                "open_target_finished",
                &correlation_id,
                Some(backend_session_id),
            )
            .duration_ms(startup_started.elapsed().as_millis()),
        );

        Ok(Self {
            _library: library,
            client,
            _client4: client4,
            client5,
            control,
            _callbacks: callbacks,
            output,
            _launched_process: launched_process,
            warnings,
            logger,
            correlation_id,
            backend_session_id: backend_session_id.to_string(),
        })
    }

    fn execute(&mut self, command: &str, timeout_ms: u64) -> Result<ExecuteBackendResult> {
        let command_text = command.to_string();
        let _dbgeng_guard = global_dbgeng_lock().lock().map_err(|_| {
            DbgFlowError::Backend("global dbgeng operation lock poisoned".to_string())
        })?;
        let started = Instant::now();
        self.logger.log(
            dbgeng_event(
                LogLevel::Info,
                "execute_started",
                &self.correlation_id,
                Some(&self.backend_session_id),
            )
            .operation(command_text.clone())
            .field("timeout_ms", timeout_ms),
        );
        {
            let mut output = self
                .output
                .lock()
                .map_err(|_| DbgFlowError::Backend("dbgeng output buffer poisoned".to_string()))?;
            output.clear();
        }

        if command.trim() == "g" {
            let mut warnings = Vec::new();
            unsafe {
                self.control
                    .SetExecutionStatus(DEBUG_STATUS_GO)
                    .map_err(|error| {
                        DbgFlowError::Backend(format!(
                            "SetExecutionStatus(DEBUG_STATUS_GO) failed: {error}"
                        ))
                    })?;
            }
            warnings.extend(wait_for_go_event(&self.control, timeout_ms)?);

            let output = self
                .output
                .lock()
                .map_err(|_| DbgFlowError::Backend("dbgeng output buffer poisoned".to_string()))?
                .clone();

            self.logger.log(
                dbgeng_event(
                    LogLevel::Info,
                    "execute_finished",
                    &self.correlation_id,
                    Some(&self.backend_session_id),
                )
                .operation("g")
                .duration_ms(started.elapsed().as_millis())
                .field("output_bytes", output.len()),
            );
            return Ok(ExecuteBackendResult { output, warnings });
        }

        if command.contains('\0') {
            return Err(DbgFlowError::Backend(
                "command contains NUL byte".to_string(),
            ));
        }
        let command = to_wide_null_str(command);
        unsafe {
            self.control
                .ExecuteWide(
                    DEBUG_OUTCTL_ALL_CLIENTS,
                    PCWSTR(command.as_ptr()),
                    DEBUG_EXECUTE_DEFAULT,
                )
                .map_err(|error| {
                    DbgFlowError::Backend(format!("IDebugControl4::ExecuteWide failed: {error}"))
                })?;
        }

        let output = self
            .output
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng output buffer poisoned".to_string()))?
            .clone();

        self.logger.log(
            dbgeng_event(
                LogLevel::Info,
                "execute_finished",
                &self.correlation_id,
                Some(&self.backend_session_id),
            )
            .operation(command_text)
            .duration_ms(started.elapsed().as_millis())
            .field("output_bytes", output.len()),
        );

        Ok(ExecuteBackendResult {
            output,
            warnings: Vec::new(),
        })
    }

    fn close(&mut self) -> Result<()> {
        let _dbgeng_guard = global_dbgeng_lock().lock().map_err(|_| {
            DbgFlowError::Backend("global dbgeng operation lock poisoned".to_string())
        })?;
        let started = Instant::now();
        self.logger.log(dbgeng_event(
            LogLevel::Info,
            "close_started",
            &self.correlation_id,
            Some(&self.backend_session_id),
        ));
        unsafe {
            self.client.EndSession(DEBUG_END_PASSIVE).map_err(|error| {
                self.logger.log(
                    dbgeng_event(
                        LogLevel::Error,
                        "end_session_failed",
                        &self.correlation_id,
                        Some(&self.backend_session_id),
                    )
                    .duration_ms(started.elapsed().as_millis())
                    .error(error.to_string()),
                );
                DbgFlowError::Backend(format!("EndSession failed: {error}"))
            })?;
            self.logger.log(dbgeng_event(
                LogLevel::Info,
                "end_session_finished",
                &self.correlation_id,
                Some(&self.backend_session_id),
            ));
            self.client5.SetOutputCallbacksWide(None).map_err(|error| {
                self.logger.log(
                    dbgeng_event(
                        LogLevel::Error,
                        "clear_output_callbacks_failed",
                        &self.correlation_id,
                        Some(&self.backend_session_id),
                    )
                    .duration_ms(started.elapsed().as_millis())
                    .error(error.to_string()),
                );
                DbgFlowError::Backend(format!("clear wide output callbacks failed: {error}"))
            })?;
        }
        self.logger.log(
            dbgeng_event(
                LogLevel::Info,
                "close_finished",
                &self.correlation_id,
                Some(&self.backend_session_id),
            )
            .duration_ms(started.elapsed().as_millis()),
        );
        Ok(())
    }
}

fn global_dbgeng_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn dbgeng_event(
    level: LogLevel,
    event: impl Into<String>,
    correlation_id: &Option<String>,
    backend_session_id: Option<&str>,
) -> LogEvent {
    let mut event = LogEvent::new(level, "dbgeng", event);
    if let Some(correlation_id) = correlation_id {
        event = event.session_id(correlation_id);
    }
    if let Some(backend_session_id) = backend_session_id {
        event = event.backend_session_id(backend_session_id.to_string());
    }
    event
}

struct LoadedDbgEng {
    module: HMODULE,
}

impl LoadedDbgEng {
    fn load(path: PathBuf) -> Result<Self> {
        let wide_path = to_wide_null(&path);
        let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
        let module = unsafe {
            LoadLibraryExW(PCWSTR(wide_path.as_ptr()), None, flags)
                .or_else(|_| LoadLibraryExW(PCWSTR(wide_path.as_ptr()), None, Default::default()))
                .map_err(|error| {
                    DbgFlowError::Backend(format!(
                        "failed to load dbgeng.dll from {}: {error}",
                        path.display()
                    ))
                })?
        };
        Ok(Self { module })
    }

    fn debug_create<T>(&self) -> Result<T>
    where
        T: Type<T, Abi = *mut c_void> + Interface,
    {
        type DebugCreateFn = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;

        let proc = unsafe { GetProcAddress(self.module, PCSTR(c"DebugCreate".as_ptr() as _)) }
            .ok_or_else(|| DbgFlowError::Backend("DebugCreate export not found".to_string()))?;
        let debug_create: DebugCreateFn = unsafe { std::mem::transmute(proc) };

        let mut raw = std::ptr::null_mut();
        unsafe {
            debug_create(&T::IID, &mut raw)
                .ok()
                .map_err(|error| DbgFlowError::Backend(format!("DebugCreate failed: {error}")))?;
            <T as Type<T>>::from_abi(raw).map_err(|error| {
                DbgFlowError::Backend(format!("DebugCreate ABI conversion failed: {error}"))
            })
        }
    }
}

#[implement(IDebugOutputCallbacksWide)]
struct DbgEngOutputCallbacks {
    output: Arc<Mutex<String>>,
}

#[allow(non_snake_case)]
impl IDebugOutputCallbacksWide_Impl for DbgEngOutputCallbacks_Impl {
    fn Output(&self, _mask: u32, text: &PCWSTR) -> windows::core::Result<()> {
        if !text.is_null() {
            let text = unsafe { pcwstr_to_string(*text) };
            if let Ok(mut output) = self.output.lock() {
                output.push_str(&text);
            }
        }
        Ok(())
    }
}

fn to_wide_null(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wait_timeout(timeout_ms: u64) -> u32 {
    timeout_ms.min(u32::MAX as u64) as u32
}

fn reply_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(5))
}

fn break_process(pid: u32) -> Result<()> {
    let access =
        PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE;
    let process = unsafe { OpenProcess(access, false, pid) }
        .map_err(|error| DbgFlowError::Backend(format!("OpenProcess for break failed: {error}")))?;
    let result = unsafe { DebugBreakProcess(process) }
        .map_err(|error| DbgFlowError::Backend(format!("DebugBreakProcess failed: {error}")));
    let _ = unsafe { CloseHandle(process) };
    result
}

fn attach_process(client: &IDebugClient4, pid: u32) -> Result<()> {
    let hr = unsafe {
        (Interface::vtable(client).AttachProcess)(
            Interface::as_raw(client),
            0,
            pid,
            DEBUG_ATTACH_DEFAULT,
        )
    };
    if hr != S_OK {
        return Err(DbgFlowError::Backend(format!(
            "AttachProcess failed: {hr:?}"
        )));
    }
    Ok(())
}

fn wait_for_go_event(control: &IDebugControl4, timeout_ms: u64) -> Result<Vec<String>> {
    let hr = unsafe {
        (Interface::vtable(control).WaitForEvent)(
            Interface::as_raw(control),
            0,
            wait_timeout(timeout_ms),
        )
    };
    if hr == S_OK {
        return Ok(Vec::new());
    }
    if hr == S_FALSE {
        return Err(DbgFlowError::Backend(
            "WaitForEvent after g timed out".to_string(),
        ));
    }
    if hr == E_UNEXPECTED {
        return Ok(vec![
            "target exited or no debuggee remains after g".to_string()
        ]);
    }
    Err(DbgFlowError::Backend(format!(
        "WaitForEvent after g failed: {hr:?}"
    )))
}

struct LaunchedProcess {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    resumed: bool,
}

impl LaunchedProcess {
    fn resume(&mut self) -> Result<()> {
        let previous_suspend_count = unsafe { ResumeThread(self.thread) };
        if previous_suspend_count == u32::MAX {
            return Err(DbgFlowError::Backend("ResumeThread failed".to_string()));
        }
        self.resumed = true;
        Ok(())
    }
}

impl Drop for LaunchedProcess {
    fn drop(&mut self) {
        if !self.resumed {
            let _ = unsafe { TerminateProcess(self.process, 1) };
        }
        let _ = unsafe { CloseHandle(self.thread) };
        let _ = unsafe { CloseHandle(self.process) };
    }
}

fn create_suspended_process(executable: &Path, args: &[String]) -> Result<LaunchedProcess> {
    let mut executable_wide = to_wide_null(executable);
    let command_line = launch_command_line(executable, args);
    let mut command_line_wide = to_wide_null_str(&command_line);
    let startup_info = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    unsafe {
        CreateProcessW(
            PCWSTR(executable_wide.as_mut_ptr()),
            Some(PWSTR(command_line_wide.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_SUSPENDED | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup_info,
            &mut process_info,
        )
    }
    .map_err(|error| DbgFlowError::Backend(format!("CreateProcessW suspended failed: {error}")))?;

    Ok(LaunchedProcess {
        process: process_info.hProcess,
        thread: process_info.hThread,
        pid: process_info.dwProcessId,
        resumed: false,
    })
}

fn to_wide_null_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn pcwstr_to_string(value: PCWSTR) -> String {
    let mut len = 0;
    let ptr = value.as_ptr();
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

fn launch_command_line(executable: &Path, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(quote_windows_arg(&executable.display().to_string()));
    parts.extend(args.iter().map(|arg| quote_windows_arg(arg)));
    parts.join(" ")
}

fn quote_windows_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if !arg.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        return arg.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
    quoted.push('"');
    quoted
}
