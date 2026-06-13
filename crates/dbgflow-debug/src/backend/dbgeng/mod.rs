use super::{
    BackendCapability, BackendEventSink, BackendExecutionEvent, BackendExecutionState, BackendInfo,
    BackendKind, BackendSession, CreateBackendSession, DebugBackend, DebugTarget,
    ExecuteBackendFailure, ExecuteBackendRequest, ExecuteBackendResult,
};
use dbgflow_common::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use dbgflow_common::{DbgFlowError, Result};
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
    IDebugOutputCallbacksWide_Impl, IDebugSymbols3, DEBUG_ATTACH_DEFAULT, DEBUG_END_PASSIVE,
    DEBUG_EXECUTE_DEFAULT, DEBUG_INTERRUPT_EXIT, DEBUG_OUTCTL_ALL_CLIENTS, DEBUG_STATUS_BREAK,
    DEBUG_STATUS_GO, DEBUG_STATUS_GO_HANDLED, DEBUG_STATUS_GO_NOT_HANDLED, DEBUG_STATUS_MASK,
    DEBUG_STATUS_NO_DEBUGGEE, DEBUG_STATUS_REVERSE_GO, DEBUG_STATUS_REVERSE_STEP_BRANCH,
    DEBUG_STATUS_REVERSE_STEP_INTO, DEBUG_STATUS_REVERSE_STEP_OVER, DEBUG_STATUS_STEP_BRANCH,
    DEBUG_STATUS_STEP_INTO, DEBUG_STATUS_STEP_OVER,
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

pub use resolver::{resolve_dbgeng, DbgEngLocation, DbgEngSource, DBGFLOW_DBGENG_DIR_ENV};

const INFINITE_WAIT_MS: u32 = u32::MAX;
const CLOSE_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const S_OK: HRESULT = HRESULT(0);
const S_FALSE: HRESULT = HRESULT(1);
const E_UNEXPECTED: HRESULT = HRESULT(0x8000FFFFu32 as i32);

pub struct DbgEngBackend {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, WorkerHandle>>,
    pending_startups: Mutex<HashMap<String, Arc<StartupCancellation>>>,
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
            pending_startups: Mutex::new(HashMap::new()),
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
        let id = format!("dbgeng-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let worker_id = id.clone();
        let logger = self.logger.clone();
        let correlation_id = request.correlation_id.clone();
        let symbol_path = request.symbol_path.clone();
        let startup_key = correlation_id.clone().unwrap_or_else(|| id.clone());
        let startup_cancel = Arc::new(StartupCancellation::default());
        self.pending_startups
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng startup map poisoned".to_string()))?
            .insert(startup_key.clone(), startup_cancel.clone());
        let join = thread::spawn(move || {
            worker_main(
                worker_id,
                request.target,
                correlation_id,
                symbol_path,
                logger,
                rx,
                init_tx,
                startup_cancel,
            )
        });

        let init_result = init_rx
            .recv()
            .map_err(|_| DbgFlowError::Backend("dbgeng worker exited during startup".to_string()));
        let _ = self
            .pending_startups
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng startup map poisoned".to_string()))?
            .remove(&startup_key);
        let init = init_result??;

        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?
            .insert(
                id.clone(),
                WorkerHandle {
                    tx,
                    join: Some(join),
                    interrupt: init.interrupt,
                },
            );

        Ok(BackendSession {
            id,
            warnings: init.warnings,
        })
    }

    fn execute(
        &self,
        request: ExecuteBackendRequest,
        event_sink: Arc<dyn BackendEventSink>,
    ) -> Result<ExecuteBackendResult> {
        self.execute_with_output_on_error(request, event_sink)
            .map_err(|failure| failure.error)
    }

    fn execute_with_output_on_error(
        &self,
        request: ExecuteBackendRequest,
        event_sink: Arc<dyn BackendEventSink>,
    ) -> std::result::Result<ExecuteBackendResult, ExecuteBackendFailure> {
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
            event_sink,
            reply: reply_tx,
        })
        .map_err(|_| DbgFlowError::Backend("dbgeng worker is not available".to_string()))?;

        reply_rx
            .recv()
            .map_err(|_| DbgFlowError::Backend("dbgeng worker is not available".to_string()))?
    }

    fn cancel_session(&self, backend_session_id: &str) -> Result<()> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?;
        let interrupt = sessions
            .get(backend_session_id)
            .map(|handle| &handle.interrupt)
            .ok_or_else(|| {
                DbgFlowError::Backend(format!("dbgeng session not found: {backend_session_id}"))
            })?;
        interrupt.interrupt()
    }

    fn cancel_startup(&self, correlation_id: &str) -> Result<()> {
        let pending = self
            .pending_startups
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng startup map poisoned".to_string()))?
            .get(correlation_id)
            .cloned()
            .ok_or_else(|| {
                DbgFlowError::Backend(format!("dbgeng startup not found: {correlation_id}"))
            })?;
        pending.cancel()
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

        let result = match reply_rx.recv_timeout(CLOSE_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.logger.log(
                    LogEvent::new(LogLevel::Error, "dbgeng", "close_timed_out")
                        .backend_session_id(backend_session_id.to_string())
                        .error("dbgeng close timed out"),
                );
                let _ = self.remove_worker_handle(backend_session_id);
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
    interrupt: DbgEngInterrupt,
}

struct WorkerInit {
    warnings: Vec<String>,
    interrupt: DbgEngInterrupt,
}

struct DbgEngInterrupt {
    control: SendableDebugControl,
    logger: Arc<dyn LogSink>,
    correlation_id: Option<String>,
    backend_session_id: String,
}

impl Clone for DbgEngInterrupt {
    fn clone(&self) -> Self {
        Self {
            control: self.control.clone(),
            logger: self.logger.clone(),
            correlation_id: self.correlation_id.clone(),
            backend_session_id: self.backend_session_id.clone(),
        }
    }
}

impl DbgEngInterrupt {
    fn interrupt(&self) -> Result<()> {
        self.logger.log(
            dbgeng_event(
                LogLevel::Info,
                "interrupt_requested",
                &self.correlation_id,
                Some(&self.backend_session_id),
            )
            .field("interrupt_flag", "DEBUG_INTERRUPT_EXIT"),
        );
        unsafe {
            self.control
                .set_interrupt(DEBUG_INTERRUPT_EXIT)
                .map_err(|error| {
                    self.logger.log(
                        dbgeng_event(
                            LogLevel::Error,
                            "interrupt_failed",
                            &self.correlation_id,
                            Some(&self.backend_session_id),
                        )
                        .field("interrupt_flag", "DEBUG_INTERRUPT_EXIT")
                        .error(error.to_string()),
                    );
                    DbgFlowError::Backend(format!(
                        "SetInterrupt(DEBUG_INTERRUPT_EXIT) failed: {error}"
                    ))
                })?;
        }
        self.logger.log(
            dbgeng_event(
                LogLevel::Info,
                "interrupt_finished",
                &self.correlation_id,
                Some(&self.backend_session_id),
            )
            .field("interrupt_result", "requested"),
        );
        Ok(())
    }
}

#[derive(Default)]
struct StartupCancellation {
    state: Mutex<StartupCancellationState>,
}

#[derive(Default)]
struct StartupCancellationState {
    cancel_requested: bool,
    interrupt: Option<DbgEngInterrupt>,
}

impl StartupCancellation {
    fn cancel(&self) -> Result<()> {
        let interrupt = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DbgFlowError::Backend("dbgeng startup cancel lock poisoned".into()))?;
            state.cancel_requested = true;
            state.interrupt.clone()
        };
        if let Some(interrupt) = interrupt {
            interrupt.interrupt()?;
        }
        Ok(())
    }

    fn install_interrupt(&self, interrupt: DbgEngInterrupt) -> Result<()> {
        let cancel_requested = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DbgFlowError::Backend("dbgeng startup cancel lock poisoned".into()))?;
            state.interrupt = Some(interrupt.clone());
            state.cancel_requested
        };
        if cancel_requested {
            interrupt.interrupt()?;
            return Err(DbgFlowError::Backend("dbgeng startup canceled".to_string()));
        }
        Ok(())
    }

    fn check_canceled(&self) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng startup cancel lock poisoned".into()))?;
        if state.cancel_requested {
            return Err(DbgFlowError::Backend("dbgeng startup canceled".to_string()));
        }
        Ok(())
    }
}

struct SendableDebugControl {
    raw: usize,
}

// dbgeng documents IDebugControl::SetInterrupt as callable from any thread. This
// wrapper owns one AddRef'd COM pointer and only exposes that interrupt call.
unsafe impl Send for SendableDebugControl {}
unsafe impl Sync for SendableDebugControl {}

impl SendableDebugControl {
    fn new(control: &IDebugControl4) -> Self {
        Self {
            raw: control.clone().into_raw() as usize,
        }
    }

    unsafe fn set_interrupt(&self, flags: u32) -> windows::core::Result<()> {
        let raw = self.raw as *mut c_void;
        let control = IDebugControl4::from_raw_borrowed(&raw).expect("stored dbgeng control");
        control.SetInterrupt(flags)
    }
}

impl Clone for SendableDebugControl {
    fn clone(&self) -> Self {
        let raw = self.raw as *mut c_void;
        let control = unsafe {
            IDebugControl4::from_raw_borrowed(&raw)
                .expect("stored dbgeng control")
                .clone()
        };
        Self {
            raw: control.into_raw() as usize,
        }
    }
}

impl Drop for SendableDebugControl {
    fn drop(&mut self) {
        unsafe {
            drop(IDebugControl4::from_raw(self.raw as *mut c_void));
        }
    }
}

enum WorkerCommand {
    Execute {
        command: String,
        event_sink: Arc<dyn BackendEventSink>,
        reply: mpsc::SyncSender<std::result::Result<ExecuteBackendResult, ExecuteBackendFailure>>,
    },
    Close {
        reply: mpsc::SyncSender<Result<()>>,
    },
}

fn worker_main(
    id: String,
    target: DebugTarget,
    correlation_id: Option<String>,
    symbol_path: Option<String>,
    logger: Arc<dyn LogSink>,
    rx: mpsc::Receiver<WorkerCommand>,
    init_tx: mpsc::SyncSender<Result<WorkerInit>>,
    startup_cancel: Arc<StartupCancellation>,
) {
    let mut session = match DbgEngSession::open_target(
        &id,
        correlation_id,
        &target,
        symbol_path,
        logger,
        startup_cancel,
    ) {
        Ok(session) => {
            let _ = init_tx.send(Ok(WorkerInit {
                warnings: session.warnings.clone(),
                interrupt: session.interrupt_handle(),
            }));
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
                event_sink,
                reply,
            } => {
                let _ = reply.send(session.execute_with_output_on_error(&command, event_sink));
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
        symbol_path: Option<String>,
        logger: Arc<dyn LogSink>,
        startup_cancel: Arc<StartupCancellation>,
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

        let result = Self::open_target_inner(
            backend_session_id,
            correlation_id.clone(),
            target,
            symbol_path,
            logger.clone(),
            startup_cancel,
            startup_started,
        );
        if let Err(error) = &result {
            logger.log(
                dbgeng_event(
                    LogLevel::Error,
                    "open_target_failed",
                    &correlation_id,
                    Some(backend_session_id),
                )
                .duration_ms(startup_started.elapsed().as_millis())
                .field("target", target)
                .error(error.to_string()),
            );
        }
        result
    }

    fn open_target_inner(
        backend_session_id: &str,
        correlation_id: Option<String>,
        target: &DebugTarget,
        symbol_path: Option<String>,
        logger: Arc<dyn LogSink>,
        startup_cancel: Arc<StartupCancellation>,
        startup_started: Instant,
    ) -> Result<Self> {
        startup_cancel.check_canceled()?;
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
        if let Some(symbol_path) = symbol_path.as_deref() {
            set_symbol_path_wide(
                &client,
                symbol_path,
                &logger,
                &correlation_id,
                backend_session_id,
            )?;
        }
        startup_cancel.install_interrupt(DbgEngInterrupt {
            control: SendableDebugControl::new(&control),
            logger: logger.clone(),
            correlation_id: correlation_id.clone(),
            backend_session_id: backend_session_id.to_string(),
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

        let target_result = (|| -> Result<()> {
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
                        startup_cancel.check_canceled()?;
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
                        startup_cancel.check_canceled()?;
                        let wait_started = Instant::now();
                        control.WaitForEvent(0, INFINITE_WAIT_MS).map_err(|error| {
                            logger.log(
                                dbgeng_event(
                                    LogLevel::Error,
                                    "wait_for_event_failed",
                                    &correlation_id,
                                    Some(backend_session_id),
                                )
                                .duration_ms(wait_started.elapsed().as_millis())
                                .error(error.to_string())
                                .field("wait_mode", "infinite"),
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
                            .field("wait_mode", "infinite"),
                        );
                    }
                    DebugTarget::Attach { pid } => {
                        startup_cancel.check_canceled()?;
                        attach_process(&client4, *pid)?;
                        break_process(*pid)?;
                        startup_cancel.check_canceled()?;
                        control.WaitForEvent(0, INFINITE_WAIT_MS).map_err(|error| {
                            DbgFlowError::Backend(format!(
                                "WaitForEvent after attach failed: {error}"
                            ))
                        })?;
                    }
                    DebugTarget::Launch { executable, args } => {
                        startup_cancel.check_canceled()?;
                        let mut launched = create_suspended_process(executable, args)?;
                        attach_process(&client4, launched.pid)?;
                        launched.resume()?;
                        launched_process = Some(launched);
                        startup_cancel.check_canceled()?;
                        control.WaitForEvent(0, INFINITE_WAIT_MS).map_err(|error| {
                            DbgFlowError::Backend(format!(
                                "WaitForEvent after launch failed: {error}"
                            ))
                        })?;
                    }
                }
            }
            Ok(())
        })();

        if let Err(error) = target_result {
            cleanup_open_target_failure(
                &client,
                &client5,
                &logger,
                &correlation_id,
                backend_session_id,
                &mut launched_process,
                &error,
            );
            return Err(error);
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

    fn interrupt_handle(&self) -> DbgEngInterrupt {
        DbgEngInterrupt {
            control: SendableDebugControl::new(&self.control),
            logger: self.logger.clone(),
            correlation_id: self.correlation_id.clone(),
            backend_session_id: self.backend_session_id.clone(),
        }
    }

    fn execute_with_output_on_error(
        &mut self,
        command: &str,
        event_sink: Arc<dyn BackendEventSink>,
    ) -> std::result::Result<ExecuteBackendResult, ExecuteBackendFailure> {
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
            .operation(command_text.clone()),
        );
        let result = (|| -> Result<ExecuteBackendResult> {
            {
                let mut output = self.output.lock().map_err(|_| {
                    DbgFlowError::Backend("dbgeng output buffer poisoned".to_string())
                })?;
                output.clear();
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
                        DbgFlowError::Backend(format!(
                            "IDebugControl4::ExecuteWide failed: {error}"
                        ))
                    })?;
            }

            let mut warnings = Vec::new();
            let mut final_state = self.query_execution_state()?;
            if final_state == Some(BackendExecutionState::Running) {
                event_sink.execution_state_changed(BackendExecutionEvent {
                    state: BackendExecutionState::Running,
                    reason: Some("dbgeng execution status running".to_string()),
                });
                warnings.extend(wait_for_execution_event(&self.control)?);
                final_state = self.query_execution_state()?;
                if let Some(state) = final_state {
                    event_sink.execution_state_changed(BackendExecutionEvent {
                        state,
                        reason: Some("dbgeng wait event returned".to_string()),
                    });
                }
            } else if final_state == Some(BackendExecutionState::Closed) {
                event_sink.execution_state_changed(BackendExecutionEvent {
                    state: BackendExecutionState::Closed,
                    reason: Some("dbgeng execution status no debuggee".to_string()),
                });
            }

            let output = self
                .output
                .lock()
                .map_err(|_| DbgFlowError::Backend("dbgeng output buffer poisoned".to_string()))?
                .clone();

            Ok(ExecuteBackendResult {
                output,
                warnings,
                final_state,
            })
        })();

        match result {
            Ok(result) => {
                self.logger.log(
                    dbgeng_event(
                        LogLevel::Info,
                        "execute_finished",
                        &self.correlation_id,
                        Some(&self.backend_session_id),
                    )
                    .operation(command_text)
                    .duration_ms(started.elapsed().as_millis())
                    .field("output_bytes", result.output.len())
                    .field("warnings_count", result.warnings.len())
                    .field(
                        "final_state",
                        result.final_state.map(|state| format!("{state:?}")),
                    ),
                );
                Ok(result)
            }
            Err(error) => {
                let final_state = self.query_execution_state().ok().flatten();
                let partial_output = self.output.lock().ok().map(|output| output.clone());
                let mut event = dbgeng_event(
                    LogLevel::Error,
                    "execute_failed",
                    &self.correlation_id,
                    Some(&self.backend_session_id),
                )
                .operation(command_text)
                .duration_ms(started.elapsed().as_millis())
                .field("final_state", final_state.map(|state| format!("{state:?}")));
                if let Some(output) = &partial_output {
                    event = event.field("output_bytes", output.len());
                }
                self.logger.log(event.error(error.to_string()));
                match partial_output {
                    Some(output) => Err(ExecuteBackendFailure::with_partial_output(error, output)),
                    None => Err(error.into()),
                }
            }
        }
    }

    fn query_execution_state(&self) -> Result<Option<BackendExecutionState>> {
        let status = unsafe { self.control.GetExecutionStatus() }.map_err(|error| {
            DbgFlowError::Backend(format!(
                "IDebugControl4::GetExecutionStatus failed: {error}"
            ))
        })?;
        Ok(execution_state_from_dbgeng_status(status))
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

fn set_symbol_path_wide(
    client: &IDebugClient,
    symbol_path: &str,
    logger: &Arc<dyn LogSink>,
    correlation_id: &Option<String>,
    backend_session_id: &str,
) -> Result<()> {
    validate_symbol_path(symbol_path)?;
    let started = Instant::now();
    logger.log(
        dbgeng_event(
            LogLevel::Info,
            "set_symbol_path_started",
            correlation_id,
            Some(backend_session_id),
        )
        .field("symbol_path_length", symbol_path.len()),
    );
    let symbols: IDebugSymbols3 = client.cast().map_err(|error| {
        logger.log(
            dbgeng_event(
                LogLevel::Error,
                "query_debug_symbols_failed",
                correlation_id,
                Some(backend_session_id),
            )
            .duration_ms(started.elapsed().as_millis())
            .error(error.to_string()),
        );
        DbgFlowError::Backend(format!("query IDebugSymbols3 failed: {error}"))
    })?;
    let wide_symbol_path = to_wide_null_str(symbol_path);
    unsafe {
        symbols
            .SetSymbolPathWide(PCWSTR(wide_symbol_path.as_ptr()))
            .map_err(|error| {
                logger.log(
                    dbgeng_event(
                        LogLevel::Error,
                        "set_symbol_path_failed",
                        correlation_id,
                        Some(backend_session_id),
                    )
                    .duration_ms(started.elapsed().as_millis())
                    .error(error.to_string()),
                );
                DbgFlowError::Backend(format!("IDebugSymbols3::SetSymbolPathWide failed: {error}"))
            })?;
    }
    logger.log(
        dbgeng_event(
            LogLevel::Info,
            "set_symbol_path_finished",
            correlation_id,
            Some(backend_session_id),
        )
        .duration_ms(started.elapsed().as_millis())
        .field("symbol_path_length", symbol_path.len()),
    );
    Ok(())
}

fn validate_symbol_path(symbol_path: &str) -> Result<()> {
    if symbol_path.trim().is_empty() {
        return Err(DbgFlowError::Backend(
            "symbol path must not be empty".to_string(),
        ));
    }
    if symbol_path
        .chars()
        .any(|ch| matches!(ch, '\0' | '\r' | '\n' | '\u{2028}' | '\u{2029}') || ch.is_control())
    {
        return Err(DbgFlowError::Backend(
            "symbol path contains unsupported control characters".to_string(),
        ));
    }
    Ok(())
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

fn wait_for_execution_event(control: &IDebugControl4) -> Result<Vec<String>> {
    let hr = unsafe {
        (Interface::vtable(control).WaitForEvent)(Interface::as_raw(control), 0, INFINITE_WAIT_MS)
    };
    if hr == S_OK {
        return Ok(Vec::new());
    }
    if hr == S_FALSE {
        return Err(DbgFlowError::Backend(
            "WaitForEvent returned without an event".to_string(),
        ));
    }
    if hr == E_UNEXPECTED {
        return Ok(vec!["target exited or no debuggee remains".to_string()]);
    }
    Err(DbgFlowError::Backend(format!(
        "WaitForEvent failed: {hr:?}"
    )))
}

fn execution_state_from_dbgeng_status(status: u32) -> Option<BackendExecutionState> {
    match status & DEBUG_STATUS_MASK {
        DEBUG_STATUS_GO
        | DEBUG_STATUS_GO_HANDLED
        | DEBUG_STATUS_GO_NOT_HANDLED
        | DEBUG_STATUS_STEP_OVER
        | DEBUG_STATUS_STEP_INTO
        | DEBUG_STATUS_STEP_BRANCH
        | DEBUG_STATUS_REVERSE_GO
        | DEBUG_STATUS_REVERSE_STEP_BRANCH
        | DEBUG_STATUS_REVERSE_STEP_OVER
        | DEBUG_STATUS_REVERSE_STEP_INTO => Some(BackendExecutionState::Running),
        DEBUG_STATUS_BREAK => Some(BackendExecutionState::Break),
        DEBUG_STATUS_NO_DEBUGGEE => Some(BackendExecutionState::Closed),
        _ => None,
    }
}

fn cleanup_open_target_failure(
    client: &IDebugClient,
    client5: &IDebugClient5,
    logger: &Arc<dyn LogSink>,
    correlation_id: &Option<String>,
    backend_session_id: &str,
    launched_process: &mut Option<LaunchedProcess>,
    error: &DbgFlowError,
) {
    logger.log(
        dbgeng_event(
            LogLevel::Warn,
            "open_target_cleanup_started",
            correlation_id,
            Some(backend_session_id),
        )
        .error(error.to_string()),
    );

    if let Some(process) = launched_process.as_mut() {
        match process.terminate() {
            Ok(()) => logger.log(dbgeng_event(
                LogLevel::Info,
                "open_target_launch_terminated",
                correlation_id,
                Some(backend_session_id),
            )),
            Err(error) => logger.log(
                dbgeng_event(
                    LogLevel::Error,
                    "open_target_launch_terminate_failed",
                    correlation_id,
                    Some(backend_session_id),
                )
                .error(error.to_string()),
            ),
        }
    }

    unsafe {
        match client.EndSession(DEBUG_END_PASSIVE) {
            Ok(()) => logger.log(dbgeng_event(
                LogLevel::Info,
                "open_target_cleanup_end_session_finished",
                correlation_id,
                Some(backend_session_id),
            )),
            Err(error) => logger.log(
                dbgeng_event(
                    LogLevel::Warn,
                    "open_target_cleanup_end_session_failed",
                    correlation_id,
                    Some(backend_session_id),
                )
                .error(error.to_string()),
            ),
        }
        match client5.SetOutputCallbacksWide(None) {
            Ok(()) => logger.log(dbgeng_event(
                LogLevel::Info,
                "open_target_cleanup_callbacks_cleared",
                correlation_id,
                Some(backend_session_id),
            )),
            Err(error) => logger.log(
                dbgeng_event(
                    LogLevel::Warn,
                    "open_target_cleanup_callbacks_failed",
                    correlation_id,
                    Some(backend_session_id),
                )
                .error(error.to_string()),
            ),
        }
    }
}

struct LaunchedProcess {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    resumed: bool,
    terminated: bool,
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

    fn terminate(&mut self) -> Result<()> {
        unsafe { TerminateProcess(self.process, 1) }
            .map_err(|error| DbgFlowError::Backend(format!("TerminateProcess failed: {error}")))?;
        self.terminated = true;
        Ok(())
    }
}

impl Drop for LaunchedProcess {
    fn drop(&mut self) {
        if !self.resumed && !self.terminated {
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
        terminated: false,
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
