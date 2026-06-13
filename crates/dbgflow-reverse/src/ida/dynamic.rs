use super::install::IdaInstall;
use super::model::{FunctionInfo, IdaInfo, IdaVersion, SegmentInfo};
use dbgflow_common::{DbgFlowError, Result};
use libloading::Library;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

const BADADDR: u64 = u64::MAX;
const MAX_SEGMENTS: i32 = 10_000;
const MAX_FUNCTIONS: usize = 1_000_000;

type InitLibrary = unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int;
type GetLibraryVersion = unsafe extern "C" fn(*mut c_int, *mut c_int, *mut c_int) -> bool;
type EnableConsoleMessages = unsafe extern "C" fn(bool);
type OpenDatabase = unsafe extern "C" fn(*const c_char, bool, *const c_char) -> c_int;
type CloseDatabase = unsafe extern "C" fn(bool);
type GetSegmQty = unsafe extern "C" fn() -> c_int;
type GetnSeg = unsafe extern "C" fn(c_int) -> *const SegmentPrefix;
type GetFuncQty = unsafe extern "C" fn() -> usize;
type GetnFunc = unsafe extern "C" fn(usize) -> *const FuncPrefix;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SegmentPrefix {
    start_ea: u64,
    end_ea: u64,
    name: u64,
    sclass: u64,
    orgbase: u64,
    align: u8,
    comb: u8,
    perm: u8,
    bitness: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FuncPrefix {
    start_ea: u64,
    end_ea: u64,
    flags: u64,
}

pub struct DynamicIdaApi {
    _ida: Library,
    _idalib: Library,
    info: IdaInfo,
    open_database: OpenDatabase,
    close_database: CloseDatabase,
    get_segm_qty: GetSegmQty,
    getnseg: GetnSeg,
    get_func_qty: GetFuncQty,
    getn_func: GetnFunc,
}

impl DynamicIdaApi {
    pub fn load_and_initialize(install: &IdaInstall) -> Result<Self> {
        unsafe {
            let ida = Library::new(&install.ida_dll).map_err(|error| {
                DbgFlowError::Backend(format!("load {}: {error}", install.ida_dll.display()))
            })?;
            let idalib = Library::new(&install.idalib_dll).map_err(|error| {
                DbgFlowError::Backend(format!("load {}: {error}", install.idalib_dll.display()))
            })?;

            let init_library: InitLibrary = *idalib
                .get(b"init_library\0")
                .map_err(|error| missing_symbol("idalib.dll", "init_library", error))?;
            let get_library_version: GetLibraryVersion = *idalib
                .get(b"get_library_version\0")
                .map_err(|error| missing_symbol("idalib.dll", "get_library_version", error))?;
            let enable_console_messages: EnableConsoleMessages = *idalib
                .get(b"enable_console_messages\0")
                .map_err(|error| missing_symbol("idalib.dll", "enable_console_messages", error))?;
            let open_database: OpenDatabase = *idalib
                .get(b"open_database\0")
                .map_err(|error| missing_symbol("idalib.dll", "open_database", error))?;
            let close_database: CloseDatabase = *idalib
                .get(b"close_database\0")
                .map_err(|error| missing_symbol("idalib.dll", "close_database", error))?;
            let get_segm_qty: GetSegmQty = *ida
                .get(b"get_segm_qty\0")
                .map_err(|error| missing_symbol("ida.dll", "get_segm_qty", error))?;
            let getnseg: GetnSeg = *ida
                .get(b"getnseg\0")
                .map_err(|error| missing_symbol("ida.dll", "getnseg", error))?;
            let get_func_qty: GetFuncQty = *ida
                .get(b"get_func_qty\0")
                .map_err(|error| missing_symbol("ida.dll", "get_func_qty", error))?;
            let getn_func: GetnFunc = *ida
                .get(b"getn_func\0")
                .map_err(|error| missing_symbol("ida.dll", "getn_func", error))?;

            let init_result = init_library(0, std::ptr::null_mut());
            if init_result != 0 {
                return Err(DbgFlowError::Backend(format!(
                    "IDA library initialization failed with code {init_result}"
                )));
            }
            enable_console_messages(false);

            let mut major = 0;
            let mut minor = 0;
            let mut build = 0;
            if !get_library_version(&mut major, &mut minor, &mut build) {
                return Err(DbgFlowError::Backend(
                    "IDA get_library_version failed".to_string(),
                ));
            }
            if major != 9 || minor != 3 {
                return Err(DbgFlowError::Backend(format!(
                    "unsupported IDA version {major}.{minor}.{build}; expected 9.3"
                )));
            }

            Ok(Self {
                _ida: ida,
                _idalib: idalib,
                info: IdaInfo {
                    install_dir: install.install_dir.clone(),
                    version: IdaVersion {
                        major,
                        minor,
                        build,
                    },
                },
                open_database,
                close_database,
                get_segm_qty,
                getnseg,
                get_func_qty,
                getn_func,
            })
        }
    }

    pub fn info(&self) -> IdaInfo {
        self.info.clone()
    }

    pub fn open_database(&self, path: &str, run_auto_analysis: bool) -> Result<()> {
        let path = CString::new(path)
            .map_err(|_| DbgFlowError::Backend("IDA target path contains NUL".to_string()))?;
        let result =
            unsafe { (self.open_database)(path.as_ptr(), run_auto_analysis, std::ptr::null()) };
        if result != 0 {
            return Err(DbgFlowError::Backend(format!(
                "IDA open_database failed with code {result}"
            )));
        }
        Ok(())
    }

    pub fn close_database(&self, save: bool) {
        unsafe {
            (self.close_database)(save);
        }
    }

    pub fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
        let count = unsafe { (self.get_segm_qty)() };
        if !(0..=MAX_SEGMENTS).contains(&count) {
            return Err(DbgFlowError::Backend(format!(
                "IDA returned invalid segment count {count}"
            )));
        }

        let mut segments = Vec::with_capacity(count as usize);
        for index in 0..count {
            let ptr = unsafe { (self.getnseg)(index) };
            if ptr.is_null() {
                return Err(DbgFlowError::Backend(format!(
                    "IDA returned null segment pointer at index {index}"
                )));
            }
            let segment = unsafe { *ptr };
            validate_range("segment", index as usize, segment.start_ea, segment.end_ea)?;
            if segment.bitness > 2 {
                return Err(DbgFlowError::Backend(format!(
                    "IDA returned invalid segment bitness {} at index {index}",
                    segment.bitness
                )));
            }
            segments.push(SegmentInfo {
                index: index as usize,
                start_ea: format_ea(segment.start_ea),
                end_ea: format_ea(segment.end_ea),
                perm: format_perm(segment.perm),
                bitness: 1u32 << (u32::from(segment.bitness) + 4),
            });
        }
        Ok(segments)
    }

    pub fn list_functions(&self) -> Result<Vec<FunctionInfo>> {
        let count = unsafe { (self.get_func_qty)() };
        if count > MAX_FUNCTIONS {
            return Err(DbgFlowError::Backend(format!(
                "IDA returned invalid function count {count}"
            )));
        }

        let mut functions = Vec::with_capacity(count);
        for index in 0..count {
            let ptr = unsafe { (self.getn_func)(index) };
            if ptr.is_null() {
                return Err(DbgFlowError::Backend(format!(
                    "IDA returned null function pointer at index {index}"
                )));
            }
            let function = unsafe { *ptr };
            validate_range("function", index, function.start_ea, function.end_ea)?;
            functions.push(FunctionInfo {
                index,
                start_ea: format_ea(function.start_ea),
                end_ea: format_ea(function.end_ea),
                flags: format!("0x{:x}", function.flags),
            });
        }
        Ok(functions)
    }
}

fn missing_symbol(dll: &str, symbol: &str, error: libloading::Error) -> DbgFlowError {
    DbgFlowError::Backend(format!("load {dll} symbol {symbol}: {error}"))
}

fn validate_range(kind: &str, index: usize, start: u64, end: u64) -> Result<()> {
    if start == BADADDR || end == BADADDR || start >= end {
        return Err(DbgFlowError::Backend(format!(
            "IDA returned invalid {kind} range at index {index}: {start:#x}..{end:#x}"
        )));
    }
    Ok(())
}

fn format_ea(value: u64) -> String {
    format!("0x{value:x}")
}

fn format_perm(perm: u8) -> String {
    let read = if perm & 4 != 0 { 'r' } else { '-' };
    let write = if perm & 2 != 0 { 'w' } else { '-' };
    let exec = if perm & 1 != 0 { 'x' } else { '-' };
    format!("{read}{write}{exec}")
}
