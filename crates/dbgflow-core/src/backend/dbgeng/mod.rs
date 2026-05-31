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
use windows::core::{implement, Interface, Type, GUID, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    IDebugClient, IDebugClient4, IDebugControl, IDebugOutputCallbacks, IDebugOutputCallbacks_Impl,
    DEBUG_END_PASSIVE, DEBUG_EXECUTE_DEFAULT, DEBUG_OUTCTL_ALL_CLIENTS,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

mod resolver;

pub use resolver::{resolve_dbgeng, DbgEngLocation, DbgEngSource};

const DEFAULT_WAIT_TIMEOUT_MS: u32 = 120_000;

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
            ],
        }
    }

    fn create_session(&self, request: CreateBackendSession) -> Result<BackendSession> {
        let DebugTarget::Dump { path } = request.target else {
            return Err(DbgFlowError::Backend(
                "DbgEngBackend only supports dump targets".to_string(),
            ));
        };

        let id = format!("dbgeng-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let worker_id = id.clone();
        let join = thread::spawn(move || worker_main(worker_id, path, rx, init_tx));

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
            reply: reply_tx,
        })
        .map_err(|_| DbgFlowError::Backend("dbgeng worker is not available".to_string()))?;

        reply_rx
            .recv_timeout(Duration::from_millis(request.timeout_ms))
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
        reply: mpsc::SyncSender<Result<ExecuteBackendResult>>,
    },
    Close {
        reply: mpsc::SyncSender<Result<()>>,
    },
}

fn worker_main(
    _id: String,
    dump_path: PathBuf,
    rx: mpsc::Receiver<WorkerCommand>,
    init_tx: mpsc::SyncSender<Result<Vec<String>>>,
) {
    let mut session = match DbgEngSession::open_dump(&dump_path) {
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
            WorkerCommand::Execute { command, reply } => {
                let _ = reply.send(session.execute(&command));
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
    warnings: Vec<String>,
}

impl DbgEngSession {
    fn open_dump(path: &Path) -> Result<Self> {
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

        unsafe {
            client.SetOutputCallbacks(&callbacks).map_err(|error| {
                DbgFlowError::Backend(format!("SetOutputCallbacks failed: {error}"))
            })?;

            let wide_path = to_wide_null(path);
            client4
                .OpenDumpFileWide(PCWSTR(wide_path.as_ptr()), 0)
                .map_err(|error| {
                    DbgFlowError::Backend(format!("OpenDumpFileWide failed: {error}"))
                })?;
            control
                .WaitForEvent(0, DEFAULT_WAIT_TIMEOUT_MS)
                .map_err(|error| DbgFlowError::Backend(format!("WaitForEvent failed: {error}")))?;
        }

        Ok(Self {
            _library: library,
            client,
            _client4: client4,
            control,
            _callbacks: callbacks,
            output,
            warnings,
        })
    }

    fn execute(&mut self, command: &str) -> Result<ExecuteBackendResult> {
        {
            let mut output = self
                .output
                .lock()
                .map_err(|_| DbgFlowError::Backend("dbgeng output buffer poisoned".to_string()))?;
            output.clear();
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
