use super::{
    BackendCapability, BackendInfo, BackendKind, BackendSession, CreateBackendSession,
    DebugBackend, DebugTarget, ExecuteBackendRequest, ExecuteBackendResult,
};
use crate::{DbgFlowError, Result};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use windows::core::{implement, Interface, Type, GUID, HRESULT, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::Debug::DebugBreakProcess;
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    IDebugClient, IDebugClient4, IDebugControl, IDebugOutputCallbacks, IDebugOutputCallbacks_Impl,
    DEBUG_ATTACH_DEFAULT, DEBUG_END_PASSIVE, DEBUG_EXECUTE_DEFAULT, DEBUG_OUTCTL_ALL_CLIENTS,
    DEBUG_STATUS_GO,
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

#[derive(Default)]
pub struct DbgEngBackend {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, WorkerHandle>>,
}

impl DbgEngBackend {
    pub fn new() -> Self {
        Self::default()
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
        let join = thread::spawn(move || worker_main(worker_id, request.target, rx, init_tx));

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
        let mut handle = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng session map poisoned".to_string()))?
            .remove(backend_session_id)
            .ok_or_else(|| {
                DbgFlowError::Backend(format!("dbgeng session not found: {backend_session_id}"))
            })?;

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        handle
            .tx
            .send(WorkerCommand::Close { reply: reply_tx })
            .map_err(|_| DbgFlowError::Backend("dbgeng worker is not available".to_string()))?;
        let result = reply_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| DbgFlowError::Backend("dbgeng close timed out".to_string()))?;

        if let Some(join) = handle.join.take() {
            let _ = join.join();
        }

        result
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
    _id: String,
    target: DebugTarget,
    rx: mpsc::Receiver<WorkerCommand>,
    init_tx: mpsc::SyncSender<Result<Vec<String>>>,
) {
    let mut session = match DbgEngSession::open_target(&target) {
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
    control: IDebugControl,
    _callbacks: IDebugOutputCallbacks,
    output: Arc<Mutex<String>>,
    _launched_process: Option<LaunchedProcess>,
    warnings: Vec<String>,
}

impl DbgEngSession {
    fn open_target(target: &DebugTarget) -> Result<Self> {
        let location = resolve_dbgeng()?;
        let mut warnings = Vec::new();
        warnings.push(format!(
            "loaded dbgeng.dll from {:?}: {}",
            location.source,
            location.path.display()
        ));
        if matches!(location.source, DbgEngSource::System32) {
            warnings.push("System32 dbgeng.dll is the last-resort fallback".to_string());
        }

        let library = LoadedDbgEng::load(location.path)?;
        let client = library.debug_create::<IDebugClient>()?;
        let client4: IDebugClient4 = client.cast().map_err(|error| {
            DbgFlowError::Backend(format!("query IDebugClient4 failed: {error}"))
        })?;
        let control: IDebugControl = client.cast().map_err(|error| {
            DbgFlowError::Backend(format!("query IDebugControl failed: {error}"))
        })?;

        let output = Arc::new(Mutex::new(String::new()));
        let callbacks: IDebugOutputCallbacks = DbgEngOutputCallbacks {
            output: Arc::clone(&output),
        }
        .into();

        let mut launched_process = None;

        unsafe {
            client.SetOutputCallbacks(&callbacks).map_err(|error| {
                DbgFlowError::Backend(format!("SetOutputCallbacks failed: {error}"))
            })?;

            match target {
                DebugTarget::Dump { path } => {
                    let wide_path = to_wide_null(path);
                    client4
                        .OpenDumpFileWide(PCWSTR(wide_path.as_ptr()), 0)
                        .map_err(|error| {
                            DbgFlowError::Backend(format!("OpenDumpFileWide failed: {error}"))
                        })?;
                    control
                        .WaitForEvent(0, DEFAULT_WAIT_TIMEOUT_MS)
                        .map_err(|error| {
                            DbgFlowError::Backend(format!("WaitForEvent failed: {error}"))
                        })?;
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

        Ok(Self {
            _library: library,
            client,
            _client4: client4,
            control,
            _callbacks: callbacks,
            output,
            _launched_process: launched_process,
            warnings,
        })
    }

    fn execute(&mut self, command: &str, timeout_ms: u64) -> Result<ExecuteBackendResult> {
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

            return Ok(ExecuteBackendResult { output, warnings });
        }

        let command = CString::new(command)
            .map_err(|_| DbgFlowError::Backend("command contains NUL byte".to_string()))?;
        unsafe {
            self.control
                .Execute(
                    DEBUG_OUTCTL_ALL_CLIENTS,
                    PCSTR(command.as_ptr() as *const u8),
                    DEBUG_EXECUTE_DEFAULT,
                )
                .map_err(|error| {
                    DbgFlowError::Backend(format!("IDebugControl::Execute failed: {error}"))
                })?;
        }

        let output = self
            .output
            .lock()
            .map_err(|_| DbgFlowError::Backend("dbgeng output buffer poisoned".to_string()))?
            .clone();

        Ok(ExecuteBackendResult {
            output,
            warnings: Vec::new(),
        })
    }

    fn close(&mut self) -> Result<()> {
        unsafe {
            self.client
                .EndSession(DEBUG_END_PASSIVE)
                .map_err(|error| DbgFlowError::Backend(format!("EndSession failed: {error}")))?;
            self.client.SetOutputCallbacks(None).map_err(|error| {
                DbgFlowError::Backend(format!("clear output callbacks failed: {error}"))
            })?;
        }
        Ok(())
    }
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

#[implement(IDebugOutputCallbacks)]
struct DbgEngOutputCallbacks {
    output: Arc<Mutex<String>>,
}

#[allow(non_snake_case)]
impl IDebugOutputCallbacks_Impl for DbgEngOutputCallbacks_Impl {
    fn Output(&self, _mask: u32, text: &PCSTR) -> windows::core::Result<()> {
        if !text.is_null() {
            let text = unsafe { CStr::from_ptr(text.as_ptr() as *const i8) }
                .to_string_lossy()
                .into_owned();
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

fn wait_for_go_event(control: &IDebugControl, timeout_ms: u64) -> Result<Vec<String>> {
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
