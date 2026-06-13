use super::install::IdaInstall;
use super::model::{
    BasicBlockInfo, BasicBlocksRequest, BasicBlocksResult, CloseDatabaseResult, CommentView,
    DecompileRequest, DecompileResult, DirectIdaCapabilities, DisassembleRequest, Disassembly,
    DisassemblyLine, ExportInfo, FunctionInfo, FunctionLookup, IdaInfo, IdaMetadata,
    IdaRichApiStatus, IdaVersion, ImportInfo, ListXrefsRequest, LookupFunctionsRequest,
    MutationItemResult, PageInfo, PageRequest, RenameRequest, SegmentInfo, SetCommentRequest,
    SetTypeRequest, StringInfo, XrefDirection, XrefInfo, XrefKind, XrefsResult,
};
use super::target::IdaTarget;
use dbgflow_common::{DbgFlowError, Result};
use libloading::Library;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

const BADADDR: u64 = u64::MAX;
const DEFAULT_PAGE_LIMIT: usize = 100;
const MAX_PAGE_LIMIT: usize = 10_000;
const MAX_SEGMENTS: i32 = 10_000;
const MAX_FUNCTIONS: usize = 1_000_000;
const DIRECT_VERSION_GATE: &str = "IDA Professional 9.3 x64";
const QSTRING_MAX_READ: usize = 16 * 1024 * 1024;

mod types {
    use super::*;

    #[repr(C)]
    #[derive(Debug)]
    pub struct RawQstring {
        data: *mut c_char,
        len: usize,
        capacity: usize,
    }

    impl RawQstring {
        pub fn new() -> Self {
            Self {
                data: std::ptr::null_mut(),
                len: 0,
                capacity: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct RawXrefBlock {
        pub from: u64,
        pub to: u64,
        pub iscode: bool,
        pub type_: u8,
        pub user: bool,
        _padding: [u8; 5],
    }

    impl Default for RawXrefBlock {
        fn default() -> Self {
            Self {
                from: BADADDR,
                to: BADADDR,
                iscode: false,
                type_: 0,
                user: false,
                _padding: [0; 5],
            }
        }
    }

    pub type QFree = unsafe extern "C" fn(*mut c_void);

    pub struct IdaQString {
        raw: RawQstring,
        qfree: QFree,
    }

    impl IdaQString {
        pub fn new(qfree: QFree) -> Self {
            Self {
                raw: RawQstring::new(),
                qfree,
            }
        }

        pub fn as_mut_ptr(&mut self) -> *mut RawQstring {
            &mut self.raw
        }

        pub fn to_string_lossy(&self) -> Option<String> {
            if self.raw.data.is_null() {
                return None;
            }
            unsafe {
                if self.raw.len > 0 && self.raw.len <= QSTRING_MAX_READ {
                    let bytes =
                        std::slice::from_raw_parts(self.raw.data.cast::<u8>(), self.raw.len);
                    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
                    return Some(String::from_utf8_lossy(bytes).into_owned());
                }
                Some(CStr::from_ptr(self.raw.data).to_string_lossy().into_owned())
            }
        }
    }

    impl Drop for IdaQString {
        fn drop(&mut self) {
            if !self.raw.data.is_null() {
                unsafe {
                    (self.qfree)(self.raw.data.cast::<c_void>());
                }
                self.raw.data = std::ptr::null_mut();
                self.raw.len = 0;
                self.raw.capacity = 0;
            }
        }
    }

    pub struct XrefBlock {
        raw: RawXrefBlock,
    }

    impl XrefBlock {
        pub fn new() -> Self {
            Self {
                raw: RawXrefBlock::default(),
            }
        }

        pub fn as_mut_ptr(&mut self) -> *mut RawXrefBlock {
            &mut self.raw
        }

        pub fn raw(&self) -> RawXrefBlock {
            self.raw
        }
    }

    #[allow(dead_code)]
    pub struct TinfoHandle;
    #[allow(dead_code)]
    pub struct HexraysFailure;
    #[allow(dead_code)]
    pub struct CfuncHandle;
}

mod abi {
    use super::types::{QFree, RawQstring, RawXrefBlock};
    use super::*;

    pub type InitLibrary = unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int;
    pub type GetLibraryVersion = unsafe extern "C" fn(*mut c_int, *mut c_int, *mut c_int) -> bool;
    pub type EnableConsoleMessages = unsafe extern "C" fn(bool);
    pub type OpenDatabase = unsafe extern "C" fn(*const c_char, bool, *const c_char) -> c_int;
    pub type CloseDatabase = unsafe extern "C" fn(bool);
    pub type GetSegmQty = unsafe extern "C" fn() -> c_int;
    pub type GetnSeg = unsafe extern "C" fn(c_int) -> *const SegmentPrefix;
    pub type GetFuncQty = unsafe extern "C" fn() -> usize;
    pub type GetnFunc = unsafe extern "C" fn(usize) -> *const FuncPrefix;

    pub type GetEaName = unsafe extern "C" fn(*mut RawQstring, u64, c_int, *const c_void) -> isize;
    pub type GetNameEa = unsafe extern "C" fn(u64, *const c_char) -> u64;
    pub type GetFuncName = unsafe extern "C" fn(*mut RawQstring, u64) -> isize;
    pub type GetSegmName = unsafe extern "C" fn(*mut RawQstring, *const c_void, c_int) -> isize;
    pub type GenerateDisasmLine = unsafe extern "C" fn(*mut RawQstring, u64, c_int) -> bool;
    pub type TagRemove = unsafe extern "C" fn(*mut RawQstring, *const c_char, c_int) -> isize;
    pub type NextHead = unsafe extern "C" fn(u64, u64) -> u64;
    pub type GetFlagsEx = unsafe extern "C" fn(u64, c_int) -> u64;
    pub type GetItemEnd = unsafe extern "C" fn(u64) -> u64;
    pub type GetStrlitContents =
        unsafe extern "C" fn(*mut RawQstring, u64, usize, i32, c_int) -> isize;
    pub type GetImportModuleQty = unsafe extern "C" fn() -> u32;
    pub type GetImportModuleName = unsafe extern "C" fn(*mut RawQstring, c_int) -> bool;
    pub type EnumImportNames =
        unsafe extern "C" fn(c_int, ImportEnumCallback, *mut c_void) -> c_int;
    pub type ImportEnumCallback =
        unsafe extern "C" fn(u64, *const c_char, u64, *mut c_void) -> c_int;
    pub type GetEntryQty = unsafe extern "C" fn() -> usize;
    pub type GetEntryOrdinal = unsafe extern "C" fn(usize) -> u64;
    pub type GetEntry = unsafe extern "C" fn(u64) -> u64;
    pub type GetEntryName = unsafe extern "C" fn(*mut RawQstring, u64) -> isize;
    pub type XrefFirst = unsafe extern "C" fn(*mut RawXrefBlock, u64, c_int) -> bool;
    pub type XrefNext = unsafe extern "C" fn(*mut RawXrefBlock) -> bool;
    pub type XrefChar = unsafe extern "C" fn(u8) -> c_char;
    pub type SetName = unsafe extern "C" fn(u64, *const c_char, c_int) -> bool;
    pub type SetCmt = unsafe extern "C" fn(u64, *const c_char, bool) -> bool;
    pub type GetCmt = unsafe extern "C" fn(*mut RawQstring, u64, bool) -> isize;
    pub type ApplyCdecl = unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int) -> bool;

    #[derive(Clone)]
    pub struct RichSymbols {
        pub qfree: Option<QFree>,
        pub qstring_layout_validated: bool,
        pub xrefblk_layout_validated: bool,
        pub get_ea_name: Option<GetEaName>,
        pub get_name_ea: Option<GetNameEa>,
        pub get_func_name: Option<GetFuncName>,
        pub get_segm_name: Option<GetSegmName>,
        pub generate_disasm_line: Option<GenerateDisasmLine>,
        pub tag_remove: Option<TagRemove>,
        pub next_head: Option<NextHead>,
        pub get_flags_ex: Option<GetFlagsEx>,
        pub get_item_end: Option<GetItemEnd>,
        pub get_strlit_contents: Option<GetStrlitContents>,
        pub get_import_module_qty: Option<GetImportModuleQty>,
        pub get_import_module_name: Option<GetImportModuleName>,
        pub enum_import_names: Option<EnumImportNames>,
        pub get_entry_qty: Option<GetEntryQty>,
        pub get_entry_ordinal: Option<GetEntryOrdinal>,
        pub get_entry: Option<GetEntry>,
        pub get_entry_name: Option<GetEntryName>,
        pub xrefblk_first_from: Option<XrefFirst>,
        pub xrefblk_next_from: Option<XrefNext>,
        pub xrefblk_first_to: Option<XrefFirst>,
        pub xrefblk_next_to: Option<XrefNext>,
        pub xrefchar: Option<XrefChar>,
        pub set_name: Option<SetName>,
        pub set_cmt: Option<SetCmt>,
        pub get_cmt: Option<GetCmt>,
        pub apply_cdecl: Option<ApplyCdecl>,
        pub missing_symbols: Vec<String>,
    }

    impl RichSymbols {
        pub unsafe fn load(ida: &Library, idalib: &Library) -> Self {
            let mut missing_symbols = Vec::new();
            Self {
                qfree: optional_any(ida, idalib, b"qfree\0", "qfree", &mut missing_symbols),
                qstring_layout_validated: false,
                xrefblk_layout_validated: false,
                get_ea_name: optional_any(
                    ida,
                    idalib,
                    b"get_ea_name\0",
                    "get_ea_name",
                    &mut missing_symbols,
                ),
                get_name_ea: optional_any(
                    ida,
                    idalib,
                    b"get_name_ea\0",
                    "get_name_ea",
                    &mut missing_symbols,
                ),
                get_func_name: optional_any(
                    ida,
                    idalib,
                    b"get_func_name\0",
                    "get_func_name",
                    &mut missing_symbols,
                ),
                get_segm_name: optional_any(
                    ida,
                    idalib,
                    b"get_segm_name\0",
                    "get_segm_name",
                    &mut missing_symbols,
                ),
                generate_disasm_line: optional_any(
                    ida,
                    idalib,
                    b"generate_disasm_line\0",
                    "generate_disasm_line",
                    &mut missing_symbols,
                ),
                tag_remove: optional_any(
                    ida,
                    idalib,
                    b"tag_remove\0",
                    "tag_remove",
                    &mut missing_symbols,
                ),
                next_head: optional_any(
                    ida,
                    idalib,
                    b"next_head\0",
                    "next_head",
                    &mut missing_symbols,
                ),
                get_flags_ex: optional_any(
                    ida,
                    idalib,
                    b"get_flags_ex\0",
                    "get_flags_ex",
                    &mut missing_symbols,
                ),
                get_item_end: optional_any(
                    ida,
                    idalib,
                    b"get_item_end\0",
                    "get_item_end",
                    &mut missing_symbols,
                ),
                get_strlit_contents: optional_any(
                    ida,
                    idalib,
                    b"get_strlit_contents\0",
                    "get_strlit_contents",
                    &mut missing_symbols,
                ),
                get_import_module_qty: optional_any(
                    ida,
                    idalib,
                    b"get_import_module_qty\0",
                    "get_import_module_qty",
                    &mut missing_symbols,
                ),
                get_import_module_name: optional_any(
                    ida,
                    idalib,
                    b"get_import_module_name\0",
                    "get_import_module_name",
                    &mut missing_symbols,
                ),
                enum_import_names: optional_any(
                    ida,
                    idalib,
                    b"enum_import_names\0",
                    "enum_import_names",
                    &mut missing_symbols,
                ),
                get_entry_qty: optional_any(
                    ida,
                    idalib,
                    b"get_entry_qty\0",
                    "get_entry_qty",
                    &mut missing_symbols,
                ),
                get_entry_ordinal: optional_any(
                    ida,
                    idalib,
                    b"get_entry_ordinal\0",
                    "get_entry_ordinal",
                    &mut missing_symbols,
                ),
                get_entry: optional_any(
                    ida,
                    idalib,
                    b"get_entry\0",
                    "get_entry",
                    &mut missing_symbols,
                ),
                get_entry_name: optional_any(
                    ida,
                    idalib,
                    b"get_entry_name\0",
                    "get_entry_name",
                    &mut missing_symbols,
                ),
                xrefblk_first_from: optional_any(
                    ida,
                    idalib,
                    b"xrefblk_t_first_from\0",
                    "xrefblk_t_first_from",
                    &mut missing_symbols,
                ),
                xrefblk_next_from: optional_any(
                    ida,
                    idalib,
                    b"xrefblk_t_next_from\0",
                    "xrefblk_t_next_from",
                    &mut missing_symbols,
                ),
                xrefblk_first_to: optional_any(
                    ida,
                    idalib,
                    b"xrefblk_t_first_to\0",
                    "xrefblk_t_first_to",
                    &mut missing_symbols,
                ),
                xrefblk_next_to: optional_any(
                    ida,
                    idalib,
                    b"xrefblk_t_next_to\0",
                    "xrefblk_t_next_to",
                    &mut missing_symbols,
                ),
                xrefchar: optional_any(
                    ida,
                    idalib,
                    b"xrefchar\0",
                    "xrefchar",
                    &mut missing_symbols,
                ),
                set_name: optional_any(
                    ida,
                    idalib,
                    b"set_name\0",
                    "set_name",
                    &mut missing_symbols,
                ),
                set_cmt: optional_any(ida, idalib, b"set_cmt\0", "set_cmt", &mut missing_symbols),
                get_cmt: optional_any(ida, idalib, b"get_cmt\0", "get_cmt", &mut missing_symbols),
                apply_cdecl: optional_any(
                    ida,
                    idalib,
                    b"apply_cdecl\0",
                    "apply_cdecl",
                    &mut missing_symbols,
                ),
                missing_symbols,
            }
        }

        pub fn qstring_available(&self) -> bool {
            self.qfree.is_some() && self.qstring_layout_validated
        }

        pub fn capabilities(&self) -> DirectIdaCapabilities {
            let qstring = self.qstring_available();
            DirectIdaCapabilities {
                names: qstring
                    && self.get_name_ea.is_some()
                    && (self.get_ea_name.is_some() || self.get_func_name.is_some()),
                disassembly: qstring
                    && self.generate_disasm_line.is_some()
                    && self.tag_remove.is_some()
                    && self.next_head.is_some()
                    && self.get_flags_ex.is_some(),
                strings: qstring
                    && self.next_head.is_some()
                    && self.get_item_end.is_some()
                    && self.get_strlit_contents.is_some(),
                imports: qstring
                    && self.get_import_module_qty.is_some()
                    && self.get_import_module_name.is_some()
                    && self.enum_import_names.is_some(),
                exports: qstring
                    && self.get_entry_qty.is_some()
                    && self.get_entry_ordinal.is_some()
                    && self.get_entry_name.is_some(),
                xrefs: self.xrefblk_layout_validated
                    && self.xrefblk_first_from.is_some()
                    && self.xrefblk_next_from.is_some()
                    && self.xrefblk_first_to.is_some()
                    && self.xrefblk_next_to.is_some(),
                basic_blocks: self.next_head.is_some(),
                comments: self.set_cmt.is_some(),
                types: self.apply_cdecl.is_some(),
                decompiler: false,
            }
        }
    }

    unsafe fn optional_any<T: Copy>(
        ida: &Library,
        idalib: &Library,
        symbol: &[u8],
        display: &str,
        missing: &mut Vec<String>,
    ) -> Option<T> {
        if let Ok(value) = ida.get::<T>(symbol) {
            return Some(*value);
        }
        if let Ok(value) = idalib.get::<T>(symbol) {
            return Some(*value);
        }
        missing.push(display.to_string());
        None
    }
}

use abi::{
    CloseDatabase, EnableConsoleMessages, GetFuncQty, GetLibraryVersion, GetSegmQty, GetnFunc,
    GetnSeg, InitLibrary, OpenDatabase, RichSymbols,
};
use types::{IdaQString, XrefBlock};

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
    rich: RichSymbols,
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
            let rich = RichSymbols::load(&ida, &idalib);

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
                rich,
            })
        }
    }

    pub fn info(&self) -> IdaInfo {
        self.info.clone()
    }

    pub fn open_database(&self, path: &str, run_auto_analysis: bool) -> Result<()> {
        let ida_path = path_for_ida_open_database(path);
        let path = CString::new(ida_path.as_ref())
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

    pub fn close_database(&self, save: bool) -> CloseDatabaseResult {
        unsafe {
            (self.close_database)(save);
        }
        CloseDatabaseResult::from_idalib_close(save)
    }

    pub fn metadata(&self, target: &IdaTarget) -> Result<IdaMetadata> {
        Ok(IdaMetadata {
            target: target.clone(),
            ida: Some(self.info()),
            segments: self.segment_count()?,
            functions: self.function_count()?,
            rich_api: self.rich_api_status(),
        })
    }

    pub fn rich_api_status(&self) -> IdaRichApiStatus {
        let capabilities = self.rich.capabilities();
        let available = any_capability(&capabilities);
        let mut warnings = Vec::new();
        if self.rich.qfree.is_some() && !self.rich.qstring_layout_validated {
            warnings.push(
                "IDA direct qstring layout validation has not passed; qstring-dependent tools are disabled"
                    .to_string(),
            );
        } else if !self.rich.qstring_available() {
            warnings.push(
                "IDA direct rich API qstring support is unavailable because qfree was not exported"
                    .to_string(),
            );
        }
        if self.rich.xrefblk_first_from.is_some() && !self.rich.xrefblk_layout_validated {
            warnings.push(
                "IDA direct xrefblk_t layout validation has not passed; xref tools are disabled"
                    .to_string(),
            );
        }
        if !capabilities.decompiler {
            warnings.push(
                "Hex-Rays direct decompiler dispatcher is unavailable in this build".to_string(),
            );
        }
        IdaRichApiStatus {
            available,
            direct_bindings: true,
            ida_version_gate: DIRECT_VERSION_GATE.to_string(),
            capabilities,
            missing_symbols: self.rich.missing_symbols.clone(),
            hexrays: Some("not_loaded".to_string()),
            warnings,
        }
    }

    pub fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
        let count = self.segment_count()?;
        let mut segments = Vec::with_capacity(count);
        for index in 0..count {
            let ptr = unsafe { (self.getnseg)(index as c_int) };
            if ptr.is_null() {
                return Err(DbgFlowError::Backend(format!(
                    "IDA returned null segment pointer at index {index}"
                )));
            }
            let segment = unsafe { *ptr };
            validate_range("segment", index, segment.start_ea, segment.end_ea)?;
            if segment.bitness > 2 {
                return Err(DbgFlowError::Backend(format!(
                    "IDA returned invalid segment bitness {} at index {index}",
                    segment.bitness
                )));
            }
            segments.push(SegmentInfo {
                index,
                start_ea: format_ea(segment.start_ea),
                end_ea: format_ea(segment.end_ea),
                size: format_ea(segment.end_ea - segment.start_ea),
                name: self.get_segment_name(ptr.cast()),
                class: None,
                perm: format_perm(segment.perm),
                bitness: 1u32 << (u32::from(segment.bitness) + 4),
            });
        }
        Ok(segments)
    }

    pub fn list_functions(&self) -> Result<Vec<FunctionInfo>> {
        let count = self.function_count()?;
        let segments = self.list_segments().unwrap_or_default();
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
            functions.push(self.function_info_from_prefix(index, function, &segments));
        }
        Ok(functions)
    }

    pub fn list_strings(&self, request: PageRequest) -> Result<Vec<StringInfo>> {
        self.require_capability("ida.list_strings", |cap| cap.strings)?;
        let mut strings = Vec::new();
        for segment in self.raw_segments()? {
            let mut ea = segment.start_ea;
            while ea < segment.end_ea {
                if let Some(value) = self.get_string_literal(ea)? {
                    let index = strings.len();
                    if page_filter_matches(request.filter.as_deref(), &[&value, &format_ea(ea)]) {
                        strings.push(StringInfo {
                            index,
                            ea: format_ea(ea),
                            length: value.len(),
                            string_type: None,
                            value,
                        });
                    }
                }
                let next = self.next_head(ea, segment.end_ea);
                if next <= ea || next == BADADDR {
                    break;
                }
                ea = next;
            }
        }
        Ok(page_vec(strings, request.offset, request.limit).0)
    }

    pub fn list_imports(&self, request: PageRequest) -> Result<Vec<ImportInfo>> {
        self.require_capability("ida.list_imports", |cap| cap.imports)?;
        let mut imports = Vec::new();
        let qty = unsafe { (self.rich.get_import_module_qty.expect("checked"))() };
        for module_index in 0..qty {
            let module = self.import_module_name(module_index as c_int);
            let mut ctx = ImportEnumContext {
                module,
                items: Vec::new(),
            };
            unsafe {
                (self.rich.enum_import_names.expect("checked"))(
                    module_index as c_int,
                    import_enum_callback,
                    (&mut ctx as *mut ImportEnumContext).cast::<c_void>(),
                );
            }
            imports.extend(ctx.items);
        }
        for (index, item) in imports.iter_mut().enumerate() {
            item.index = index;
        }
        let filtered = imports
            .into_iter()
            .filter(|item| {
                let ea = item.ea.as_str();
                let module = item.module.as_deref().unwrap_or_default();
                let name = item.name.as_deref().unwrap_or_default();
                page_filter_matches(request.filter.as_deref(), &[ea, module, name])
            })
            .collect::<Vec<_>>();
        Ok(page_vec(filtered, request.offset, request.limit).0)
    }

    pub fn list_exports(&self, request: PageRequest) -> Result<Vec<ExportInfo>> {
        self.require_capability("ida.list_exports", |cap| cap.exports)?;
        let qty = unsafe { (self.rich.get_entry_qty.expect("checked"))() };
        let mut exports = Vec::new();
        for index in 0..qty {
            let ordinal = unsafe { (self.rich.get_entry_ordinal.expect("checked"))(index) };
            let name = self.entry_name(ordinal);
            let ea = self
                .rich
                .get_entry
                .map(|get_entry| unsafe { get_entry(ordinal) })
                .filter(|ea| *ea != BADADDR)
                .map(format_ea)
                .unwrap_or_else(|| "unknown".to_string());
            let info = ExportInfo {
                index,
                ea,
                name,
                ordinal: Some(ordinal),
            };
            let fields = [
                info.ea.as_str(),
                info.name.as_deref().unwrap_or_default(),
                "",
            ];
            if page_filter_matches(request.filter.as_deref(), &fields) {
                exports.push(info);
            }
        }
        Ok(page_vec(exports, request.offset, request.limit).0)
    }

    pub fn lookup_functions(&self, request: LookupFunctionsRequest) -> Result<Vec<FunctionLookup>> {
        let functions = self.list_functions()?;
        Ok(request
            .queries
            .into_iter()
            .map(|query| {
                let found = self
                    .resolve_ea(&query)
                    .ok()
                    .and_then(|ea| find_function_by_ea(&functions, ea))
                    .or_else(|| find_function_by_name(&functions, &query));
                match found {
                    Some(function) => FunctionLookup {
                        query,
                        function: Some(function),
                        error: None,
                    },
                    None => FunctionLookup {
                        query,
                        function: None,
                        error: Some("function not found".to_string()),
                    },
                }
            })
            .collect())
    }

    pub fn disassemble(&self, request: DisassembleRequest) -> Result<Disassembly> {
        self.require_capability("ida.disassemble", |cap| cap.disassembly)?;
        let ea = self.resolve_ea(&request.target)?;
        let functions = self.list_functions()?;
        let function = find_function_by_ea(&functions, ea);
        let start = function
            .as_ref()
            .and_then(|function| parse_ea(&function.start_ea))
            .unwrap_or(ea);
        let end = function
            .as_ref()
            .and_then(|function| parse_ea(&function.end_ea))
            .unwrap_or_else(|| self.next_head(ea, BADADDR).max(ea.saturating_add(1)));
        let (addresses, page) = self.collect_heads_page(start, end, request.offset, request.limit);
        let mut lines = Vec::with_capacity(addresses.len());
        for ea in addresses {
            if let Some(text) = self.disassembly_line(ea)? {
                lines.push(DisassemblyLine {
                    ea: format_ea(ea),
                    text,
                    label: self.get_ea_name(ea),
                    comments: self.comments_for_ea(ea),
                    refs: Vec::new(),
                });
            }
        }
        Ok(Disassembly {
            target: request.target,
            function,
            page: PageInfo {
                returned: lines.len(),
                ..page
            },
            lines,
            error: None,
        })
    }

    pub fn decompile(&self, request: DecompileRequest) -> Result<DecompileResult> {
        self.require_capability("ida.decompile", |cap| cap.decompiler)?;
        let ea = self.resolve_ea(&request.target)?;
        let function = find_function_by_ea(&self.list_functions()?, ea);
        Ok(DecompileResult {
            target: request.target,
            function,
            code: None,
            refs: Vec::new(),
            error: Some("Hex-Rays direct decompiler dispatcher is unavailable".to_string()),
        })
    }

    pub fn list_xrefs(&self, request: ListXrefsRequest) -> Result<XrefsResult> {
        self.require_capability("ida.list_xrefs", |cap| cap.xrefs)?;
        let target = request.target.clone();
        let ea = self.resolve_ea(&request.target)?;
        let mut xrefs = Vec::new();
        if matches!(request.direction, XrefDirection::From | XrefDirection::Both) {
            self.collect_xrefs(ea, "from", &request.kind, &mut xrefs);
        }
        if matches!(request.direction, XrefDirection::To | XrefDirection::Both) {
            self.collect_xrefs(ea, "to", &request.kind, &mut xrefs);
        }
        let total = xrefs.len();
        let (xrefs, page) = page_vec(xrefs, request.offset, request.limit);
        Ok(XrefsResult {
            target,
            xrefs,
            page: PageInfo {
                total,
                returned: page.returned,
                ..page
            },
            error: None,
        })
    }

    pub fn list_basic_blocks(&self, request: BasicBlocksRequest) -> Result<BasicBlocksResult> {
        self.require_capability("ida.list_basic_blocks", |cap| cap.basic_blocks)?;
        let ea = self.resolve_ea(&request.target)?;
        let function = find_function_by_ea(&self.list_functions()?, ea);
        let Some(function) = function else {
            return Ok(BasicBlocksResult {
                target: request.target,
                function: None,
                blocks: Vec::new(),
                error: Some("target is not inside a function".to_string()),
            });
        };
        Ok(BasicBlocksResult {
            target: request.target,
            blocks: vec![BasicBlockInfo {
                id: 0,
                start_ea: function.start_ea.clone(),
                end_ea: function.end_ea.clone(),
                successors: Vec::new(),
                predecessors: Vec::new(),
            }],
            function: Some(function),
            error: None,
        })
    }

    pub fn rename(&self, request: RenameRequest) -> Result<Vec<MutationItemResult>> {
        let mut results = Vec::with_capacity(request.items.len());
        for item in request.items {
            let result = match self.rename_one(
                &item.target,
                &item.name,
                request.dry_run,
                request.allow_overwrite,
            ) {
                Ok(result) => result,
                Err(error) => MutationItemResult {
                    target: item.target,
                    old: None,
                    new: Some(item.name),
                    success: false,
                    dry_run: request.dry_run,
                    error: Some(error.to_string()),
                },
            };
            results.push(result);
        }
        Ok(results)
    }

    pub fn set_comment(&self, request: SetCommentRequest) -> Result<Vec<MutationItemResult>> {
        let mut results = Vec::with_capacity(request.items.len());
        for item in request.items {
            let result = match self.set_comment_one(
                &item.target,
                &item.comment,
                request.repeatable,
                &request.view,
            ) {
                Ok(result) => result,
                Err(error) => MutationItemResult {
                    target: item.target,
                    old: None,
                    new: Some(item.comment),
                    success: false,
                    dry_run: false,
                    error: Some(error.to_string()),
                },
            };
            results.push(result);
        }
        Ok(results)
    }

    pub fn set_type(&self, request: SetTypeRequest) -> Result<Vec<MutationItemResult>> {
        let mut results = Vec::with_capacity(request.items.len());
        for item in request.items {
            let result = match self.set_type_one(&item.target, &item.type_text, request.dry_run) {
                Ok(result) => result,
                Err(error) => MutationItemResult {
                    target: item.target,
                    old: None,
                    new: Some(item.type_text),
                    success: false,
                    dry_run: request.dry_run,
                    error: Some(error.to_string()),
                },
            };
            results.push(result);
        }
        Ok(results)
    }

    fn segment_count(&self) -> Result<usize> {
        let count = unsafe { (self.get_segm_qty)() };
        if !(0..=MAX_SEGMENTS).contains(&count) {
            return Err(DbgFlowError::Backend(format!(
                "IDA returned invalid segment count {count}"
            )));
        }
        Ok(count as usize)
    }

    fn function_count(&self) -> Result<usize> {
        let count = unsafe { (self.get_func_qty)() };
        if count > MAX_FUNCTIONS {
            return Err(DbgFlowError::Backend(format!(
                "IDA returned invalid function count {count}"
            )));
        }
        Ok(count)
    }

    fn raw_segments(&self) -> Result<Vec<SegmentPrefix>> {
        let count = self.segment_count()?;
        let mut segments = Vec::with_capacity(count);
        for index in 0..count {
            let ptr = unsafe { (self.getnseg)(index as c_int) };
            if ptr.is_null() {
                return Err(DbgFlowError::Backend(format!(
                    "IDA returned null segment pointer at index {index}"
                )));
            }
            let segment = unsafe { *ptr };
            validate_range("segment", index, segment.start_ea, segment.end_ea)?;
            segments.push(segment);
        }
        Ok(segments)
    }

    fn function_info_from_prefix(
        &self,
        index: usize,
        function: FuncPrefix,
        segments: &[SegmentInfo],
    ) -> FunctionInfo {
        let segment = segments.iter().find_map(|segment| {
            let start = parse_ea(&segment.start_ea)?;
            let end = parse_ea(&segment.end_ea)?;
            (function.start_ea >= start && function.start_ea < end)
                .then(|| segment.name.clone())
                .flatten()
        });
        FunctionInfo {
            index,
            start_ea: format_ea(function.start_ea),
            end_ea: format_ea(function.end_ea),
            size: format_ea(function.end_ea - function.start_ea),
            name: self.get_func_name(function.start_ea),
            segment,
            prototype: None,
            flags: format!("0x{:x}", function.flags),
        }
    }

    fn require_capability(
        &self,
        tool: &str,
        predicate: impl FnOnce(&DirectIdaCapabilities) -> bool,
    ) -> Result<()> {
        let capabilities = self.rich.capabilities();
        if predicate(&capabilities) {
            return Ok(());
        }
        let missing = if self.rich.missing_symbols.is_empty() {
            "no missing symbol details were reported".to_string()
        } else {
            self.rich.missing_symbols.join(", ")
        };
        Err(DbgFlowError::Backend(format!(
            "{tool} is unsupported by the IDA direct rich API for {}; missing or unavailable symbols: {missing}",
            DIRECT_VERSION_GATE
        )))
    }

    fn resolve_ea(&self, target: &str) -> Result<u64> {
        if let Some(ea) = parse_ea(target) {
            return Ok(ea);
        }
        let get_name_ea = self
            .rich
            .get_name_ea
            .ok_or_else(|| self.unsupported_error("name lookup"))?;
        let target = CString::new(target)
            .map_err(|_| DbgFlowError::Backend("IDA target contains NUL".to_string()))?;
        let ea = unsafe { get_name_ea(BADADDR, target.as_ptr()) };
        if ea == BADADDR {
            return Err(DbgFlowError::Backend(
                "IDA target was not found".to_string(),
            ));
        }
        Ok(ea)
    }

    fn unsupported_error(&self, capability: &str) -> DbgFlowError {
        let missing = if self.rich.missing_symbols.is_empty() {
            "no missing symbol details were reported".to_string()
        } else {
            self.rich.missing_symbols.join(", ")
        };
        DbgFlowError::Backend(format!(
            "IDA direct rich API capability {capability} is unavailable; missing or unavailable symbols: {missing}"
        ))
    }

    fn qstring(&self) -> Option<IdaQString> {
        self.rich.qfree.map(IdaQString::new)
    }

    fn get_ea_name(&self, ea: u64) -> Option<String> {
        let get_ea_name = self.rich.get_ea_name?;
        let mut out = self.qstring()?;
        let len = unsafe { get_ea_name(out.as_mut_ptr(), ea, 0, std::ptr::null()) };
        (len > 0).then(|| out.to_string_lossy()).flatten()
    }

    fn get_func_name(&self, ea: u64) -> Option<String> {
        let get_func_name = self.rich.get_func_name?;
        let mut out = self.qstring()?;
        let len = unsafe { get_func_name(out.as_mut_ptr(), ea) };
        (len > 0).then(|| out.to_string_lossy()).flatten()
    }

    fn get_segment_name(&self, segment: *const c_void) -> Option<String> {
        let get_segm_name = self.rich.get_segm_name?;
        let mut out = self.qstring()?;
        let len = unsafe { get_segm_name(out.as_mut_ptr(), segment, 0) };
        (len > 0).then(|| out.to_string_lossy()).flatten()
    }

    fn get_string_literal(&self, ea: u64) -> Result<Option<String>> {
        let get_item_end = self.rich.get_item_end.expect("checked");
        let get_strlit_contents = self.rich.get_strlit_contents.expect("checked");
        let end = unsafe { get_item_end(ea) };
        if end <= ea || end == BADADDR {
            return Ok(None);
        }
        let len = (end - ea) as usize;
        let mut out = self
            .qstring()
            .ok_or_else(|| self.unsupported_error("qstring"))?;
        let result = unsafe { get_strlit_contents(out.as_mut_ptr(), ea, len, 0, 0) };
        if result <= 0 {
            return Ok(None);
        }
        Ok(out.to_string_lossy().filter(|value| !value.is_empty()))
    }

    fn import_module_name(&self, module_index: c_int) -> Option<String> {
        let get_import_module_name = self.rich.get_import_module_name?;
        let mut out = self.qstring()?;
        let ok = unsafe { get_import_module_name(out.as_mut_ptr(), module_index) };
        ok.then(|| out.to_string_lossy()).flatten()
    }

    fn entry_name(&self, ordinal: u64) -> Option<String> {
        let get_entry_name = self.rich.get_entry_name?;
        let mut out = self.qstring()?;
        let len = unsafe { get_entry_name(out.as_mut_ptr(), ordinal) };
        (len > 0).then(|| out.to_string_lossy()).flatten()
    }

    fn next_head(&self, ea: u64, max_ea: u64) -> u64 {
        self.rich
            .next_head
            .map(|next_head| unsafe { next_head(ea, max_ea) })
            .unwrap_or(BADADDR)
    }

    fn collect_heads_page(
        &self,
        start: u64,
        end: u64,
        offset: usize,
        limit: Option<usize>,
    ) -> (Vec<u64>, PageInfo) {
        let limit = normalize_limit(limit);
        let mut ea = start;
        let mut total = 0usize;
        let mut addresses = Vec::new();
        let mut has_more = false;
        while ea < end && ea != BADADDR {
            if total >= offset && addresses.len() < limit {
                addresses.push(ea);
            } else if total >= offset.saturating_add(limit) {
                has_more = true;
                total += 1;
                break;
            }
            total += 1;
            let next = self.next_head(ea, end);
            if next <= ea || next == BADADDR {
                break;
            }
            ea = next;
        }
        let returned = addresses.len();
        (
            addresses,
            PageInfo {
                offset,
                limit,
                total,
                returned,
                next_offset: has_more.then_some(offset + returned),
            },
        )
    }

    fn disassembly_line(&self, ea: u64) -> Result<Option<String>> {
        let generate_disasm_line = self.rich.generate_disasm_line.expect("checked");
        let mut colored = self
            .qstring()
            .ok_or_else(|| self.unsupported_error("qstring"))?;
        let ok = unsafe { generate_disasm_line(colored.as_mut_ptr(), ea, 0) };
        if !ok {
            return Ok(None);
        }
        let Some(colored_text) = colored.to_string_lossy() else {
            return Ok(None);
        };
        Ok(Some(self.remove_tags(&colored_text)?))
    }

    fn remove_tags(&self, text: &str) -> Result<String> {
        let Some(tag_remove) = self.rich.tag_remove else {
            return Ok(text.to_string());
        };
        let input = CString::new(text)
            .map_err(|_| DbgFlowError::Backend("IDA disassembly contains NUL".to_string()))?;
        let mut out = self
            .qstring()
            .ok_or_else(|| self.unsupported_error("qstring"))?;
        let len = unsafe { tag_remove(out.as_mut_ptr(), input.as_ptr(), 0) };
        if len <= 0 {
            return Ok(text.to_string());
        }
        Ok(out.to_string_lossy().unwrap_or_else(|| text.to_string()))
    }

    fn comments_for_ea(&self, ea: u64) -> Vec<String> {
        let mut comments = Vec::new();
        if let Some(comment) = self.get_comment(ea, false) {
            comments.push(comment);
        }
        if let Some(comment) = self.get_comment(ea, true) {
            comments.push(comment);
        }
        comments
    }

    fn get_comment(&self, ea: u64, repeatable: bool) -> Option<String> {
        let get_cmt = self.rich.get_cmt?;
        let mut out = self.qstring()?;
        let len = unsafe { get_cmt(out.as_mut_ptr(), ea, repeatable) };
        (len > 0).then(|| out.to_string_lossy()).flatten()
    }

    fn collect_xrefs(
        &self,
        ea: u64,
        direction: &str,
        kind_filter: &XrefKind,
        out: &mut Vec<XrefInfo>,
    ) {
        let (first, next) = if direction == "from" {
            (
                self.rich.xrefblk_first_from.expect("checked"),
                self.rich.xrefblk_next_from.expect("checked"),
            )
        } else {
            (
                self.rich.xrefblk_first_to.expect("checked"),
                self.rich.xrefblk_next_to.expect("checked"),
            )
        };
        let mut block = XrefBlock::new();
        let mut ok = unsafe { first(block.as_mut_ptr(), ea, 0) };
        while ok {
            let raw = block.raw();
            let include = match kind_filter {
                XrefKind::Any => true,
                XrefKind::Code => raw.iscode,
                XrefKind::Data => !raw.iscode,
            };
            if include {
                let type_char = self
                    .rich
                    .xrefchar
                    .map(|xrefchar| unsafe { xrefchar(raw.type_) as u8 as char })
                    .unwrap_or('?');
                out.push(XrefInfo {
                    direction: Some(direction.to_string()),
                    from: format_ea(raw.from),
                    to: format_ea(raw.to),
                    kind: if raw.iscode { "code" } else { "data" }.to_string(),
                    type_name: Some(type_char.to_string()),
                    user: raw.user,
                    function: None,
                });
            }
            ok = unsafe { next(block.as_mut_ptr()) };
        }
    }

    fn rename_one(
        &self,
        target: &str,
        name: &str,
        dry_run: bool,
        allow_overwrite: bool,
    ) -> Result<MutationItemResult> {
        let set_name = self
            .rich
            .set_name
            .ok_or_else(|| self.unsupported_error("rename"))?;
        let ea = self.resolve_ea(target)?;
        if !allow_overwrite {
            if let Ok(existing) = self.resolve_ea(name) {
                if existing != ea {
                    return Ok(MutationItemResult {
                        target: target.to_string(),
                        old: self.get_ea_name(ea),
                        new: Some(name.to_string()),
                        success: false,
                        dry_run,
                        error: Some(format!("name already exists at {}", format_ea(existing))),
                    });
                }
            }
        }
        let old = self.get_ea_name(ea);
        if dry_run {
            return Ok(MutationItemResult {
                target: target.to_string(),
                old,
                new: Some(name.to_string()),
                success: true,
                dry_run,
                error: None,
            });
        }
        let name_c = CString::new(name)
            .map_err(|_| DbgFlowError::Backend("IDA name contains NUL".to_string()))?;
        let ok = unsafe { set_name(ea, name_c.as_ptr(), 0) };
        Ok(MutationItemResult {
            target: target.to_string(),
            old,
            new: Some(name.to_string()),
            success: ok,
            dry_run,
            error: (!ok).then(|| "set_name returned false".to_string()),
        })
    }

    fn set_comment_one(
        &self,
        target: &str,
        comment: &str,
        repeatable: bool,
        view: &CommentView,
    ) -> Result<MutationItemResult> {
        let set_cmt = self
            .rich
            .set_cmt
            .ok_or_else(|| self.unsupported_error("comments"))?;
        let ea = self.resolve_ea(target)?;
        let old = self.get_comment(ea, repeatable);
        let comment_c = CString::new(comment)
            .map_err(|_| DbgFlowError::Backend("IDA comment contains NUL".to_string()))?;
        let disassembly_requested = matches!(view, CommentView::Disassembly | CommentView::Both);
        let decompiler_requested = matches!(view, CommentView::Decompiler | CommentView::Both);
        let mut success = true;
        let mut error = None;
        if disassembly_requested {
            success = unsafe { set_cmt(ea, comment_c.as_ptr(), repeatable) };
            if !success {
                error = Some("set_cmt returned false".to_string());
            }
        }
        if decompiler_requested {
            error = Some(match error {
                Some(existing) => {
                    format!("{existing}; Hex-Rays decompiler comment view is unavailable")
                }
                None => "Hex-Rays decompiler comment view is unavailable".to_string(),
            });
            success = false;
        }
        Ok(MutationItemResult {
            target: target.to_string(),
            old,
            new: Some(comment.to_string()),
            success,
            dry_run: false,
            error,
        })
    }

    fn set_type_one(
        &self,
        target: &str,
        type_text: &str,
        dry_run: bool,
    ) -> Result<MutationItemResult> {
        let apply_cdecl = self
            .rich
            .apply_cdecl
            .ok_or_else(|| self.unsupported_error("types"))?;
        let ea = self.resolve_ea(target)?;
        if dry_run {
            return Ok(MutationItemResult {
                target: target.to_string(),
                old: None,
                new: Some(type_text.to_string()),
                success: false,
                dry_run,
                error: Some(
                    "set_type dry_run validation is unavailable without a validated parse_decl/tinfo direct binding"
                        .to_string(),
                ),
            });
        }
        let type_c = CString::new(type_text)
            .map_err(|_| DbgFlowError::Backend("IDA type declaration contains NUL".to_string()))?;
        let ok = unsafe { apply_cdecl(std::ptr::null_mut(), ea, type_c.as_ptr(), 0) };
        Ok(MutationItemResult {
            target: target.to_string(),
            old: None,
            new: Some(type_text.to_string()),
            success: ok,
            dry_run,
            error: (!ok).then(|| "apply_cdecl returned false".to_string()),
        })
    }
}

struct ImportEnumContext {
    module: Option<String>,
    items: Vec<ImportInfo>,
}

unsafe extern "C" fn import_enum_callback(
    ea: u64,
    name: *const c_char,
    ordinal: u64,
    param: *mut c_void,
) -> c_int {
    if param.is_null() {
        return 0;
    }
    let ctx = &mut *(param.cast::<ImportEnumContext>());
    let name = (!name.is_null()).then(|| CStr::from_ptr(name).to_string_lossy().into_owned());
    ctx.items.push(ImportInfo {
        index: 0,
        ea: format_ea(ea),
        module: ctx.module.clone(),
        name,
        ordinal: (ordinal != 0).then_some(ordinal),
    });
    1
}

fn any_capability(capabilities: &DirectIdaCapabilities) -> bool {
    capabilities.names
        || capabilities.disassembly
        || capabilities.strings
        || capabilities.imports
        || capabilities.exports
        || capabilities.xrefs
        || capabilities.basic_blocks
        || capabilities.comments
        || capabilities.types
        || capabilities.decompiler
}

fn path_for_ida_open_database(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(path) = path.strip_prefix("\\\\?\\UNC\\") {
        return std::borrow::Cow::Owned(format!("\\\\{path}"));
    }
    if let Some(path) = path.strip_prefix("\\\\?\\") {
        return std::borrow::Cow::Borrowed(path);
    }
    std::borrow::Cow::Borrowed(path)
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

fn parse_ea(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).ok();
    }
    u64::from_str_radix(value, 16)
        .ok()
        .or_else(|| value.parse::<u64>().ok())
}

fn find_function_by_ea(functions: &[FunctionInfo], ea: u64) -> Option<FunctionInfo> {
    functions.iter().find_map(|function| {
        let start = parse_ea(&function.start_ea)?;
        let end = parse_ea(&function.end_ea)?;
        (ea >= start && ea < end).then(|| function.clone())
    })
}

fn find_function_by_name(functions: &[FunctionInfo], query: &str) -> Option<FunctionInfo> {
    functions.iter().find_map(|function| {
        function
            .name
            .as_ref()
            .is_some_and(|name| name.eq_ignore_ascii_case(query))
            .then(|| function.clone())
    })
}

fn normalize_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

fn page_vec<T>(items: Vec<T>, offset: usize, limit: Option<usize>) -> (Vec<T>, PageInfo) {
    let limit = normalize_limit(limit);
    let total = items.len();
    let returned = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let returned_len = returned.len();
    (
        returned,
        PageInfo {
            offset,
            limit,
            total,
            returned: returned_len,
            next_offset: (offset + returned_len < total).then_some(offset + returned_len),
        },
    )
}

fn page_filter_matches(filter: Option<&str>, fields: &[&str]) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let filter = filter.to_ascii_lowercase();
    fields
        .iter()
        .any(|field| field.to_ascii_lowercase().contains(&filter))
}

fn format_perm(perm: u8) -> String {
    let read = if perm & 4 != 0 { 'r' } else { '-' };
    let write = if perm & 2 != 0 { 'w' } else { '-' };
    let exec = if perm & 1 != 0 { 'x' } else { '-' };
    format!("{read}{write}{exec}")
}

#[cfg(test)]
mod tests {
    use super::path_for_ida_open_database;

    #[test]
    fn ida_open_database_path_strips_drive_verbatim_prefix() {
        let path = path_for_ida_open_database(r"\\?\C:\samples\a.exe");

        assert_eq!(path, r"C:\samples\a.exe");
    }

    #[test]
    fn ida_open_database_path_converts_unc_verbatim_prefix() {
        let path = path_for_ida_open_database(r"\\?\UNC\server\share\a.exe");

        assert_eq!(path, r"\\server\share\a.exe");
    }

    #[test]
    fn ida_open_database_path_leaves_normal_path_unchanged() {
        let path = path_for_ida_open_database(r"C:\samples\a.exe");

        assert_eq!(path, r"C:\samples\a.exe");
    }
}
