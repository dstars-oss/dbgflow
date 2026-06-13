use super::{
    CollectorFactory, EtwEventSet, ProfileCollector, ProfileCollectorConfig, ProfileCollectorKind,
};
#[cfg(test)]
use crate::profile::EtwProfileScope;
use dbgflow_common::{DbgFlowError, Result};
#[cfg(not(windows))]
use std::path::Path;

mod settings;
mod stack;

use settings::{native_etw_settings, NativeEtwSettings};
use stack::{event_matches_target, hex32, hex64, resolve_stack_addresses, ModuleInterval};

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

#[cfg(not(windows))]
impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        _output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        let _ = native_etw_settings(config)?;
        Err(DbgFlowError::Backend(
            "native ETW profiling is only supported on Windows".to_string(),
        ))
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use dbgflow_common::artifacts::{ArtifactKind, ArtifactRef};
    use serde::Serialize;
    use serde_json::json;
    use std::alloc::{alloc_zeroed, dealloc, Layout};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::Write;
    use std::mem::{align_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::Mutex;
    use uuid::Uuid;
    use windows::core::{GUID, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR,
    };
    use windows::Win32::System::Diagnostics::Etw::{
        CloseTrace, ControlTraceW, FileIoGuid, ImageLoadGuid, OpenTraceW, ProcessGuid,
        ProcessTrace, PropertyStruct, StartTraceW, TdhGetEventInformation, TdhGetProperty,
        ThreadGuid, TraceSetInformation, TraceStackTracingInfo, CLASSIC_EVENT_ID,
        CONTROLTRACE_HANDLE, EVENT_HEADER_EXT_TYPE_STACK_TRACE32,
        EVENT_HEADER_EXT_TYPE_STACK_TRACE64, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
        EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_FLAG_DISK_FILE_IO, EVENT_TRACE_FLAG_DISK_IO,
        EVENT_TRACE_FLAG_FILE_IO, EVENT_TRACE_FLAG_FILE_IO_INIT, EVENT_TRACE_FLAG_IMAGE_LOAD,
        EVENT_TRACE_FLAG_PROCESS, EVENT_TRACE_FLAG_THREAD, EVENT_TRACE_LOGFILEW,
        EVENT_TRACE_PROPERTIES, EVENT_TRACE_SYSTEM_LOGGER_MODE, EVENT_TRACE_TYPE_END,
        EVENT_TRACE_TYPE_LOAD, EVENT_TRACE_TYPE_START, PROCESS_TRACE_MODE_EVENT_RECORD,
        PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROPERTY_DATA_DESCRIPTOR, TDH_INTYPE_ANSISTRING,
        TDH_INTYPE_COUNTEDANSISTRING, TDH_INTYPE_COUNTEDSTRING,
        TDH_INTYPE_MANIFEST_COUNTEDANSISTRING, TDH_INTYPE_MANIFEST_COUNTEDSTRING,
        TDH_INTYPE_NONNULLTERMINATEDANSISTRING, TDH_INTYPE_NONNULLTERMINATEDSTRING,
        TDH_INTYPE_REVERSEDCOUNTEDANSISTRING, TDH_INTYPE_REVERSEDCOUNTEDSTRING,
        TDH_INTYPE_UNICODESTRING, TRACE_EVENT_INFO, WNODE_FLAG_TRACED_GUID,
    };

    const STACK_WALK_GUID: GUID = GUID::from_u128(0xdef2fe46_7bd6_4b80_bd94_f57fe20d0ce3);
    const STACK_WALK_EVENT_TYPE: u32 = 32;
    const MAX_PENDING_STACK_TIMESTAMPS: usize = 4096;
    const MAX_PENDING_STACKS_PER_TIMESTAMP: usize = 32;

    #[derive(Debug, Default)]
    pub struct NativeEtwCollectorFactory;

    impl CollectorFactory for NativeEtwCollectorFactory {
        fn create(
            &self,
            config: &ProfileCollectorConfig,
            output_dir: &Path,
        ) -> Result<Box<dyn ProfileCollector>> {
            let settings = native_etw_settings(config)?;
            Ok(Box::new(NativeEtwCollector::new(
                output_dir.join("trace.etl"),
                output_dir.join("process.jsonl"),
                output_dir.join("file_io.jsonl"),
                output_dir.join("summary.json"),
                settings,
            )))
        }
    }

    struct NativeEtwCollector {
        trace_path: PathBuf,
        process_path: PathBuf,
        file_io_path: PathBuf,
        summary_path: PathBuf,
        settings: NativeEtwSettings,
        state: Mutex<NativeEtwState>,
    }

    #[derive(Debug, Default)]
    struct NativeEtwState {
        session_name: Option<String>,
        target_pid: Option<u32>,
    }

    impl NativeEtwCollector {
        fn new(
            trace_path: PathBuf,
            process_path: PathBuf,
            file_io_path: PathBuf,
            summary_path: PathBuf,
            settings: NativeEtwSettings,
        ) -> Self {
            Self {
                trace_path,
                process_path,
                file_io_path,
                summary_path,
                settings,
                state: Mutex::new(NativeEtwState::default()),
            }
        }
    }

    impl ProfileCollector for NativeEtwCollector {
        fn name(&self) -> &str {
            "native_etw"
        }

        fn kind(&self) -> ProfileCollectorKind {
            ProfileCollectorKind::NativeEtw
        }

        fn start(&self) -> Result<super::super::CollectorStart> {
            let mut state = self.state.lock().map_err(|_| {
                DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
            })?;
            if state.session_name.is_some() {
                return Err(DbgFlowError::Backend(
                    "native ETW collector already started".to_string(),
                ));
            }

            let session_name = format!("dbgflow-profile-{}", Uuid::new_v4());
            start_trace_session(&session_name, &self.trace_path, self.settings)?;
            state.session_name = Some(session_name);
            state.target_pid = None;
            Ok(super::super::CollectorStart {
                warnings: Vec::new(),
            })
        }

        fn target_started(&self, target_pid: u32) {
            if let Ok(mut state) = self.state.lock() {
                state.target_pid = Some(target_pid);
            }
        }

        fn stop(&self, target_pid: Option<u32>) -> Result<super::super::CollectorStop> {
            let (session_name, notified_pid) = {
                let state = self.state.lock().map_err(|_| {
                    DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
                })?;
                let Some(session_name) = state.session_name.clone() else {
                    return Ok(super::super::CollectorStop {
                        artifacts: Vec::new(),
                        warnings: vec!["native ETW collector was not started".to_string()],
                    });
                };
                (session_name, state.target_pid)
            };

            let mut warnings = stop_trace_session(&session_name, &self.trace_path)?;
            {
                let mut state = self.state.lock().map_err(|_| {
                    DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
                })?;
                if state.session_name.as_deref() == Some(session_name.as_str()) {
                    state.session_name = None;
                }
            }
            let target_pid = notified_pid.or(target_pid);
            match target_pid {
                Some(pid) => {
                    let summary = match post_process_trace(
                        &self.trace_path,
                        &self.process_path,
                        &self.file_io_path,
                        pid,
                        self.settings,
                    ) {
                        Ok(summary) => summary,
                        Err(error) => {
                            warnings.push(format!("native ETW post-processing failed: {error}"));
                            if let Err(error) = ensure_empty_event_set_files(
                                self.settings,
                                &self.process_path,
                                &self.file_io_path,
                            ) {
                                warnings.push(format!(
                                    "native ETW events fallback write failed: {error}"
                                ));
                            }
                            EtwSummary::empty(Some(pid), self.settings, warnings.clone())
                        }
                    };
                    let new_summary_warnings = summary
                        .warnings
                        .iter()
                        .filter(|warning| !warnings.contains(warning))
                        .cloned()
                        .collect::<Vec<_>>();
                    warnings.extend(new_summary_warnings);
                    if let Err(error) = write_summary(&self.summary_path, &summary) {
                        warnings.push(format!("native ETW summary write failed: {error}"));
                    }
                }
                None => {
                    warnings.push(
                        "native ETW post-processing skipped because the target pid is unavailable"
                            .to_string(),
                    );
                    if let Err(error) = ensure_empty_event_set_files(
                        self.settings,
                        &self.process_path,
                        &self.file_io_path,
                    ) {
                        warnings.push(format!("native ETW events fallback write failed: {error}"));
                    }
                    let summary = EtwSummary::empty(None, self.settings, warnings.clone());
                    if let Err(error) = write_summary(&self.summary_path, &summary) {
                        warnings.push(format!("native ETW summary write failed: {error}"));
                    }
                }
            }

            Ok(super::super::CollectorStop {
                artifacts: collector_artifacts(
                    self.settings,
                    &self.trace_path,
                    &self.process_path,
                    &self.file_io_path,
                    &self.summary_path,
                ),
                warnings,
            })
        }

        fn cleanup(&self) -> Result<()> {
            let session_name = self
                .state
                .lock()
                .map_err(|_| {
                    DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
                })?
                .session_name
                .clone();
            if let Some(session_name) = session_name {
                if stop_trace_session(&session_name, &self.trace_path).is_ok() {
                    let mut state = self.state.lock().map_err(|_| {
                        DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
                    })?;
                    if state.session_name.as_deref() == Some(session_name.as_str()) {
                        state.session_name = None;
                    }
                }
            }
            Ok(())
        }
    }

    fn start_trace_session(
        session_name: &str,
        trace_path: &Path,
        settings: NativeEtwSettings,
    ) -> Result<()> {
        let session_name_w = wide_null(OsStr::new(session_name));
        let trace_path_w = wide_null(trace_path.as_os_str());
        let properties_size = size_of::<EVENT_TRACE_PROPERTIES>()
            + session_name_w.len() * size_of::<u16>()
            + trace_path_w.len() * size_of::<u16>();
        let mut buffer = EtwPropertiesBuffer::new(properties_size)?;
        let properties = buffer.properties();

        unsafe {
            (*properties).Wnode.BufferSize = properties_size as u32;
            (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*properties).Wnode.ClientContext = 1;
            (*properties).LogFileMode =
                EVENT_TRACE_FILE_MODE_SEQUENTIAL | EVENT_TRACE_SYSTEM_LOGGER_MODE;
            (*properties).EnableFlags = enable_flags(settings);
            (*properties).BufferSize = 1024;
            (*properties).MinimumBuffers = 64;
            (*properties).MaximumBuffers = 256;
            (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            (*properties).LogFileNameOffset = (size_of::<EVENT_TRACE_PROPERTIES>()
                + session_name_w.len() * size_of::<u16>())
                as u32;

            copy_wide_to_buffer(
                buffer.as_bytes_mut(),
                (*properties).LoggerNameOffset as usize,
                &session_name_w,
            );
            copy_wide_to_buffer(
                buffer.as_bytes_mut(),
                (*properties).LogFileNameOffset as usize,
                &trace_path_w,
            );

            let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
            let status = StartTraceW(&mut handle, PCWSTR(session_name_w.as_ptr()), properties);
            if status != ERROR_SUCCESS {
                return Err(etw_error("StartTraceW", status));
            }
            if settings.stacks_enabled {
                if let Err(error) = enable_stack_tracing(handle, settings) {
                    let _ = stop_trace_session(session_name, trace_path);
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    fn enable_flags(
        settings: NativeEtwSettings,
    ) -> windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_FLAG {
        let mut flags = windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_FLAG(0);
        if settings.includes(EtwEventSet::Process) {
            flags |=
                EVENT_TRACE_FLAG_PROCESS | EVENT_TRACE_FLAG_THREAD | EVENT_TRACE_FLAG_IMAGE_LOAD;
        }
        if settings.includes(EtwEventSet::FileIo) {
            flags |= EVENT_TRACE_FLAG_DISK_IO
                | EVENT_TRACE_FLAG_DISK_FILE_IO
                | EVENT_TRACE_FLAG_FILE_IO_INIT
                | EVENT_TRACE_FLAG_FILE_IO;
        }
        flags
    }

    unsafe fn enable_stack_tracing(
        handle: CONTROLTRACE_HANDLE,
        settings: NativeEtwSettings,
    ) -> Result<()> {
        let mut events = Vec::new();
        if settings.includes(EtwEventSet::Process) {
            events.extend([
                classic_event(ProcessGuid, EVENT_TRACE_TYPE_START),
                classic_event(ProcessGuid, EVENT_TRACE_TYPE_END),
                classic_event(ThreadGuid, EVENT_TRACE_TYPE_START),
                classic_event(ThreadGuid, EVENT_TRACE_TYPE_END),
                classic_event(ImageLoadGuid, EVENT_TRACE_TYPE_LOAD),
                classic_event(ImageLoadGuid, EVENT_TRACE_TYPE_END),
            ]);
        }
        if settings.includes(EtwEventSet::FileIo) {
            events.extend(file_io_stack_events());
        }
        if events.is_empty() {
            return Ok(());
        }
        let status = TraceSetInformation(
            handle,
            TraceStackTracingInfo,
            events.as_ptr().cast(),
            (events.len() * size_of::<CLASSIC_EVENT_ID>()) as u32,
        );
        if status != ERROR_SUCCESS {
            return Err(etw_error(
                "TraceSetInformation TraceStackTracingInfo",
                status,
            ));
        }
        Ok(())
    }

    fn classic_event(event_guid: GUID, event_type: u32) -> CLASSIC_EVENT_ID {
        CLASSIC_EVENT_ID {
            EventGuid: event_guid,
            Type: event_type as u8,
            Reserved: [0; 7],
        }
    }

    const FILE_IO_NAME: u32 = 0;
    const FILE_IO_FILE_CREATE_NAME: u32 = 32;
    const FILE_IO_FILE_DELETE_NAME: u32 = 35;
    const FILE_IO_RUNDOWN: u32 = 36;
    const FILE_IO_CREATE: u32 = 64;
    const FILE_IO_CLEANUP: u32 = 65;
    const FILE_IO_CLOSE: u32 = 66;
    const FILE_IO_READ: u32 = 67;
    const FILE_IO_WRITE: u32 = 68;
    const FILE_IO_SET_INFO: u32 = 69;
    const FILE_IO_DELETE: u32 = 70;
    const FILE_IO_RENAME: u32 = 71;
    const FILE_IO_DIR_ENUM: u32 = 72;
    const FILE_IO_FLUSH: u32 = 73;
    const FILE_IO_QUERY_INFO: u32 = 74;
    const FILE_IO_FS_CONTROL: u32 = 75;
    const FILE_IO_OP_END: u32 = 76;
    const FILE_IO_DIR_NOTIFY: u32 = 77;

    fn file_io_stack_events() -> [CLASSIC_EVENT_ID; 16] {
        [
            classic_event(FileIoGuid, FILE_IO_NAME),
            classic_event(FileIoGuid, FILE_IO_FILE_CREATE_NAME),
            classic_event(FileIoGuid, FILE_IO_FILE_DELETE_NAME),
            classic_event(FileIoGuid, FILE_IO_RUNDOWN),
            classic_event(FileIoGuid, FILE_IO_CREATE),
            classic_event(FileIoGuid, FILE_IO_CLEANUP),
            classic_event(FileIoGuid, FILE_IO_READ),
            classic_event(FileIoGuid, FILE_IO_WRITE),
            classic_event(FileIoGuid, FILE_IO_SET_INFO),
            classic_event(FileIoGuid, FILE_IO_DELETE),
            classic_event(FileIoGuid, FILE_IO_RENAME),
            classic_event(FileIoGuid, FILE_IO_DIR_ENUM),
            classic_event(FileIoGuid, FILE_IO_FLUSH),
            classic_event(FileIoGuid, FILE_IO_QUERY_INFO),
            classic_event(FileIoGuid, FILE_IO_FS_CONTROL),
            classic_event(FileIoGuid, FILE_IO_DIR_NOTIFY),
        ]
    }

    fn stop_trace_session(session_name: &str, trace_path: &Path) -> Result<Vec<String>> {
        let session_name_w = wide_null(OsStr::new(session_name));
        let trace_path_w = wide_null(trace_path.as_os_str());
        let properties_size = size_of::<EVENT_TRACE_PROPERTIES>()
            + session_name_w.len() * size_of::<u16>()
            + trace_path_w.len() * size_of::<u16>();
        let mut buffer = EtwPropertiesBuffer::new(properties_size)?;
        let properties = buffer.properties();

        unsafe {
            (*properties).Wnode.BufferSize = properties_size as u32;
            (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            (*properties).LogFileNameOffset = (size_of::<EVENT_TRACE_PROPERTIES>()
                + session_name_w.len() * size_of::<u16>())
                as u32;
            copy_wide_to_buffer(
                buffer.as_bytes_mut(),
                (*properties).LoggerNameOffset as usize,
                &session_name_w,
            );
            copy_wide_to_buffer(
                buffer.as_bytes_mut(),
                (*properties).LogFileNameOffset as usize,
                &trace_path_w,
            );

            let status = ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                PCWSTR(session_name_w.as_ptr()),
                properties,
                EVENT_TRACE_CONTROL_STOP,
            );
            if status == ERROR_MORE_DATA {
                return Ok(vec![
                    "ControlTraceW stop returned ERROR_MORE_DATA after stopping the ETW session"
                        .to_string(),
                ]);
            }
            if status != ERROR_SUCCESS {
                return Err(etw_error("ControlTraceW stop", status));
            }
        }

        Ok(Vec::new())
    }

    #[derive(Debug, Serialize)]
    struct EtwSummary {
        target_pid: Option<u32>,
        requested_event_sets: Vec<&'static str>,
        stacks_enabled: bool,
        event_sets: BTreeMap<&'static str, EtwEventSetSummary>,
        warnings: Vec<String>,
    }

    #[derive(Debug, Serialize)]
    struct EtwEventSetSummary {
        event_counts: BTreeMap<&'static str, u64>,
        stack_frames_total: u64,
        stack_frames_resolved: u64,
        stack_frames_unresolved: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_path_resolved: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_path_unresolved: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        matched_op_end_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unmatched_op_end_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        incomplete_io_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reused_irp_without_op_end_count: Option<u64>,
    }

    #[derive(Debug, Default)]
    struct StackStats {
        total: u64,
        resolved: u64,
        unresolved: u64,
    }

    impl EtwEventSetSummary {
        fn lifecycle(event_counts: BTreeMap<&'static str, u64>, stack: StackStats) -> Self {
            Self {
                event_counts,
                stack_frames_total: stack.total,
                stack_frames_resolved: stack.resolved,
                stack_frames_unresolved: stack.unresolved,
                file_path_resolved: None,
                file_path_unresolved: None,
                matched_op_end_count: None,
                unmatched_op_end_count: None,
                incomplete_io_count: None,
                reused_irp_without_op_end_count: None,
            }
        }

        fn file_io(
            event_counts: BTreeMap<&'static str, u64>,
            stack: StackStats,
            file_path_resolved: u64,
            file_path_unresolved: u64,
            matched_op_end_count: u64,
            unmatched_op_end_count: u64,
            incomplete_io_count: u64,
            reused_irp_without_op_end_count: u64,
        ) -> Self {
            Self {
                event_counts,
                stack_frames_total: stack.total,
                stack_frames_resolved: stack.resolved,
                stack_frames_unresolved: stack.unresolved,
                file_path_resolved: Some(file_path_resolved),
                file_path_unresolved: Some(file_path_unresolved),
                matched_op_end_count: Some(matched_op_end_count),
                unmatched_op_end_count: Some(unmatched_op_end_count),
                incomplete_io_count: Some(incomplete_io_count),
                reused_irp_without_op_end_count: Some(reused_irp_without_op_end_count),
            }
        }
    }

    impl EtwSummary {
        fn empty(
            target_pid: Option<u32>,
            settings: NativeEtwSettings,
            warnings: Vec<String>,
        ) -> Self {
            let mut event_sets = BTreeMap::new();
            if settings.includes(EtwEventSet::Process) {
                event_sets.insert(
                    "process",
                    EtwEventSetSummary::lifecycle(lifecycle_count_map(), StackStats::default()),
                );
            }
            if settings.includes(EtwEventSet::FileIo) {
                event_sets.insert(
                    "file_io",
                    EtwEventSetSummary::file_io(
                        file_io_count_map(),
                        StackStats::default(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                    ),
                );
            }
            Self {
                target_pid,
                requested_event_sets: settings.event_set_names(),
                stacks_enabled: settings.stacks_enabled,
                event_sets,
                warnings,
            }
        }
    }

    #[derive(Debug, Serialize)]
    struct LifecycleEvent {
        sequence: u64,
        event_set: &'static str,
        event: &'static str,
        pid: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        tid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_base: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_size: Option<String>,
        stack: Vec<String>,
    }

    #[derive(Debug, Serialize)]
    struct FileIoEvent {
        sequence: u64,
        event_set: &'static str,
        event: &'static str,
        pid: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        tid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ttid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path_source: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_object: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_key: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        irp_ptr: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        offset: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        io_size: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        io_flags: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        create_options: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_attributes: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        share_access: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        info_class: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extra_info: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        nt_status: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        stack: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        completion_pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        completion_tid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        completion_sequence: Option<u64>,
    }

    #[derive(Debug)]
    struct DecodedLifecycleEvent {
        event_timestamp: i64,
        event_thread_id: u32,
        event: &'static str,
        pid: u32,
        tid: Option<u32>,
        parent_pid: Option<u32>,
        image_path: Option<String>,
        image_base: Option<u64>,
        image_size: Option<u64>,
        stack_addresses: Vec<u64>,
    }

    #[derive(Debug)]
    struct DecodedFileIoEvent {
        event_timestamp: i64,
        event_thread_id: u32,
        event: &'static str,
        pid: u32,
        ttid: Option<u32>,
        path: Option<String>,
        path_source: Option<&'static str>,
        file_object: Option<u64>,
        file_key: Option<u64>,
        irp_ptr: Option<u64>,
        offset: Option<u64>,
        io_size: Option<u64>,
        io_flags: Option<u32>,
        create_options: Option<u32>,
        file_attributes: Option<u32>,
        share_access: Option<u32>,
        info_class: Option<u32>,
        extra_info: Option<u64>,
        nt_status: Option<u32>,
        stack_addresses: Vec<u64>,
    }

    #[derive(Debug, Clone)]
    struct DecodedStackWalkEvent {
        event_timestamp: i64,
        stack_process: u32,
        stack_thread: u32,
        stack_addresses: Vec<u64>,
    }

    #[derive(Debug, Clone, Copy)]
    enum StackTargetKind {
        Lifecycle,
        FileIo,
    }

    #[derive(Debug, Clone, Copy)]
    struct StackTarget {
        kind: StackTargetKind,
        index: usize,
    }

    struct EtwProcessor {
        target_pid: u32,
        settings: NativeEtwSettings,
        lifecycle_events: Vec<LifecycleEvent>,
        lifecycle_timestamps: Vec<i64>,
        lifecycle_header_threads: Vec<u32>,
        lifecycle_stack_addresses: Vec<Vec<u64>>,
        file_io_events: Vec<FileIoEvent>,
        file_io_timestamps: Vec<i64>,
        file_io_header_threads: Vec<u32>,
        file_io_stack_addresses: Vec<Vec<u64>>,
        pending_file_irps: BTreeMap<u64, usize>,
        ignored_file_irps: BTreeSet<u64>,
        event_indices_by_timestamp: BTreeMap<i64, Vec<StackTarget>>,
        pending_stacks_by_timestamp: BTreeMap<i64, Vec<DecodedStackWalkEvent>>,
        file_paths_by_pointer: BTreeMap<u64, String>,
        modules: Vec<ModuleInterval>,
        warnings: Vec<String>,
        lifecycle_event_counts: BTreeMap<&'static str, u64>,
        file_io_event_counts: BTreeMap<&'static str, u64>,
        lifecycle_stack_stats: StackStats,
        file_io_stack_stats: StackStats,
        file_path_resolved: u64,
        file_path_unresolved: u64,
        file_io_raw_sequence: u64,
        matched_op_end_count: u64,
        unmatched_op_end_count: u64,
        reused_irp_without_op_end_count: u64,
        stack_walk_events: u64,
        matched_stack_walk_events: u64,
        dropped_pending_stack_walk_events: u64,
        lifecycle_match_samples: Vec<String>,
        stack_walk_match_samples: Vec<String>,
    }

    impl EtwProcessor {
        fn new(target_pid: u32, settings: NativeEtwSettings) -> Self {
            Self {
                target_pid,
                settings,
                lifecycle_events: Vec::new(),
                lifecycle_timestamps: Vec::new(),
                lifecycle_header_threads: Vec::new(),
                lifecycle_stack_addresses: Vec::new(),
                file_io_events: Vec::new(),
                file_io_timestamps: Vec::new(),
                file_io_header_threads: Vec::new(),
                file_io_stack_addresses: Vec::new(),
                pending_file_irps: BTreeMap::new(),
                ignored_file_irps: BTreeSet::new(),
                event_indices_by_timestamp: BTreeMap::new(),
                pending_stacks_by_timestamp: BTreeMap::new(),
                file_paths_by_pointer: BTreeMap::new(),
                modules: Vec::new(),
                warnings: Vec::new(),
                lifecycle_event_counts: lifecycle_count_map(),
                file_io_event_counts: file_io_count_map(),
                lifecycle_stack_stats: StackStats::default(),
                file_io_stack_stats: StackStats::default(),
                file_path_resolved: 0,
                file_path_unresolved: 0,
                file_io_raw_sequence: 0,
                matched_op_end_count: 0,
                unmatched_op_end_count: 0,
                reused_irp_without_op_end_count: 0,
                stack_walk_events: 0,
                matched_stack_walk_events: 0,
                dropped_pending_stack_walk_events: 0,
                lifecycle_match_samples: Vec::new(),
                stack_walk_match_samples: Vec::new(),
            }
        }

        unsafe fn process_record(&mut self, record: *mut EVENT_RECORD) {
            if let Some(stack) = decode_stack_walk_event(record) {
                self.process_stack_walk_event(stack);
                return;
            }
            if let Some(decoded) = decode_lifecycle_event(record, &mut self.warnings) {
                self.process_lifecycle_event(decoded);
                return;
            }
            if self.settings.includes(EtwEventSet::FileIo) {
                if let Some(decoded) = decode_file_io_event(record, &mut self.warnings) {
                    self.process_file_io_event(decoded);
                }
            }
        }

        fn process_lifecycle_event(&mut self, decoded: DecodedLifecycleEvent) {
            if !event_matches_target(decoded.pid, self.target_pid) {
                return;
            }
            self.prune_pending_stacks_before(decoded.event_timestamp);

            match decoded.event {
                "image_load" => {
                    if let (Some(base), Some(size)) = (decoded.image_base, decoded.image_size) {
                        self.record_module_load(
                            decoded.event_timestamp,
                            base,
                            size,
                            decoded
                                .image_path
                                .clone()
                                .unwrap_or_else(|| format!("image_{base:x}")),
                        );
                    }
                }
                "image_unload" => {
                    if let Some(base) = decoded.image_base {
                        self.record_module_unload(
                            decoded.event_timestamp,
                            base,
                            decoded.image_size,
                        );
                    }
                }
                _ => {}
            }

            if !self.settings.includes(EtwEventSet::Process) {
                return;
            }

            if let Some(count) = self.lifecycle_event_counts.get_mut(decoded.event) {
                *count += 1;
            }
            if self.lifecycle_match_samples.len() < 5 {
                self.lifecycle_match_samples.push(format!(
                    "{}:ts={},header_tid={},pid={}",
                    decoded.event, decoded.event_timestamp, decoded.event_thread_id, decoded.pid
                ));
            }
            let sequence = self.lifecycle_events.len() as u64 + 1;
            let event_index = self.lifecycle_events.len();
            self.lifecycle_events.push(LifecycleEvent {
                sequence,
                event_set: "process",
                event: decoded.event,
                pid: decoded.pid,
                tid: decoded.tid,
                parent_pid: decoded.parent_pid,
                image_path: decoded.image_path.clone(),
                image_base: decoded.image_base.map(hex64),
                image_size: decoded.image_size.map(hex64),
                stack: Vec::new(),
            });
            self.lifecycle_timestamps.push(decoded.event_timestamp);
            self.lifecycle_header_threads.push(decoded.event_thread_id);
            self.lifecycle_stack_addresses
                .push(decoded.stack_addresses.clone());
            self.event_indices_by_timestamp
                .entry(decoded.event_timestamp)
                .or_default()
                .push(StackTarget {
                    kind: StackTargetKind::Lifecycle,
                    index: event_index,
                });

            if let Some(pending) = self
                .pending_stacks_by_timestamp
                .remove(&decoded.event_timestamp)
            {
                for stack in pending {
                    self.attach_stack_walk_event(stack);
                }
            }
        }

        fn process_file_io_event(&mut self, decoded: DecodedFileIoEvent) {
            if !self.file_io_event_matches_target(&decoded) {
                return;
            }
            self.file_io_raw_sequence += 1;
            self.prune_pending_stacks_before(decoded.event_timestamp);

            if decoded.event == "op_end" {
                self.merge_file_io_completion(decoded);
                return;
            }
            if decoded.event == "close" {
                self.ignore_file_io_close(decoded);
                return;
            }

            self.cache_file_path(&decoded);
            let (path, path_source) = self.resolve_file_path(&decoded);
            if path.is_some() {
                self.file_path_resolved += 1;
            } else {
                self.file_path_unresolved += 1;
            }

            if let Some(count) = self.file_io_event_counts.get_mut(decoded.event) {
                *count += 1;
            }

            let sequence = self.file_io_events.len() as u64 + 1;
            let event_index = self.file_io_events.len();
            let invalidates_file_object = decoded.event == "cleanup";
            self.file_io_events.push(FileIoEvent {
                sequence,
                event_set: "file_io",
                event: decoded.event,
                pid: decoded.pid,
                tid: Some(decoded.event_thread_id),
                ttid: decoded.ttid,
                path,
                path_source,
                file_object: decoded.file_object.map(hex64),
                file_key: decoded.file_key.map(hex64),
                irp_ptr: decoded.irp_ptr.map(hex64),
                offset: decoded.offset.map(hex64),
                io_size: decoded.io_size,
                io_flags: decoded.io_flags.map(hex32),
                create_options: decoded.create_options.map(hex32),
                file_attributes: decoded.file_attributes.map(hex32),
                share_access: decoded.share_access.map(hex32),
                info_class: decoded.info_class,
                extra_info: decoded.extra_info.map(hex64),
                nt_status: decoded.nt_status.map(hex32),
                stack: Vec::new(),
                completion_pid: None,
                completion_tid: None,
                completion_sequence: None,
            });
            self.file_io_timestamps.push(decoded.event_timestamp);
            self.file_io_header_threads.push(decoded.event_thread_id);
            self.file_io_stack_addresses
                .push(decoded.stack_addresses.clone());
            self.event_indices_by_timestamp
                .entry(decoded.event_timestamp)
                .or_default()
                .push(StackTarget {
                    kind: StackTargetKind::FileIo,
                    index: event_index,
                });
            if let Some(irp_ptr) = decoded.irp_ptr {
                if self
                    .pending_file_irps
                    .insert(irp_ptr, event_index)
                    .is_some()
                {
                    self.reused_irp_without_op_end_count += 1;
                }
            }

            if let Some(pending) = self
                .pending_stacks_by_timestamp
                .remove(&decoded.event_timestamp)
            {
                for stack in pending {
                    self.attach_stack_walk_event(stack);
                }
            }
            if invalidates_file_object {
                self.remove_file_path(&decoded);
            }
        }

        fn ignore_file_io_close(&mut self, decoded: DecodedFileIoEvent) {
            if let Some(irp_ptr) = decoded.irp_ptr {
                if self.pending_file_irps.remove(&irp_ptr).is_some() {
                    self.reused_irp_without_op_end_count += 1;
                }
                self.ignored_file_irps.insert(irp_ptr);
            }
            self.remove_file_path(&decoded);
        }

        fn merge_file_io_completion(&mut self, decoded: DecodedFileIoEvent) {
            let Some(irp_ptr) = decoded.irp_ptr else {
                self.unmatched_op_end_count += 1;
                return;
            };
            let Some(event_index) = self.pending_file_irps.remove(&irp_ptr) else {
                if self.ignored_file_irps.remove(&irp_ptr) {
                    return;
                }
                self.unmatched_op_end_count += 1;
                return;
            };
            let Some(event) = self.file_io_events.get_mut(event_index) else {
                self.unmatched_op_end_count += 1;
                return;
            };

            self.matched_op_end_count += 1;
            if let Some(extra_info) = decoded.extra_info {
                event.extra_info = Some(hex64(extra_info));
            }
            if let Some(nt_status) = decoded.nt_status {
                event.nt_status = Some(hex32(nt_status));
            }
            event.completion_pid = Some(decoded.pid);
            event.completion_tid = Some(decoded.event_thread_id);
            event.completion_sequence = Some(self.file_io_raw_sequence);
        }

        fn file_io_event_matches_target(&self, decoded: &DecodedFileIoEvent) -> bool {
            if decoded.event == "op_end" {
                if decoded
                    .irp_ptr
                    .map(|irp_ptr| {
                        self.pending_file_irps.contains_key(&irp_ptr)
                            || self.ignored_file_irps.contains(&irp_ptr)
                    })
                    .unwrap_or(false)
                {
                    return true;
                }
                return event_matches_target(decoded.pid, self.target_pid);
            }
            event_matches_target(decoded.pid, self.target_pid)
        }

        fn cache_file_path(&mut self, decoded: &DecodedFileIoEvent) {
            let Some(path) = decoded.path.as_ref().filter(|path| !path.is_empty()) else {
                return;
            };
            if let Some(file_object) = decoded.file_object {
                self.file_paths_by_pointer.insert(file_object, path.clone());
            }
            if let Some(file_key) = decoded.file_key {
                self.file_paths_by_pointer.insert(file_key, path.clone());
            }
        }

        fn remove_file_path(&mut self, decoded: &DecodedFileIoEvent) {
            if let Some(file_object) = decoded.file_object {
                self.file_paths_by_pointer.remove(&file_object);
            }
            if let Some(file_key) = decoded.file_key {
                self.file_paths_by_pointer.remove(&file_key);
            }
        }

        fn resolve_file_path(
            &self,
            decoded: &DecodedFileIoEvent,
        ) -> (Option<String>, Option<&'static str>) {
            if let Some(path) = decoded.path.as_ref().filter(|path| !path.is_empty()) {
                return (Some(path.clone()), decoded.path_source);
            }
            if let Some(path) = decoded
                .file_object
                .and_then(|pointer| self.file_paths_by_pointer.get(&pointer))
            {
                return (Some(path.clone()), Some("file_object_cache"));
            }
            if let Some(path) = decoded
                .file_key
                .and_then(|pointer| self.file_paths_by_pointer.get(&pointer))
            {
                return (Some(path.clone()), Some("file_key_cache"));
            }
            (None, None)
        }

        fn record_module_load(&mut self, timestamp: i64, base: u64, size: u64, name: String) {
            self.modules.push(ModuleInterval {
                base,
                size,
                name,
                loaded_at: timestamp,
                unloaded_at: None,
            });
        }

        fn record_module_unload(&mut self, timestamp: i64, base: u64, size: Option<u64>) {
            if let Some(module) = self.modules.iter_mut().rev().find(|module| {
                module.base == base
                    && module.unloaded_at.is_none()
                    && size
                        .map(|image_size| image_size == module.size)
                        .unwrap_or(true)
            }) {
                module.unloaded_at = Some(timestamp);
            }
        }

        fn process_stack_walk_event(&mut self, stack: DecodedStackWalkEvent) {
            self.stack_walk_events += 1;
            if self.stack_walk_match_samples.len() < 5 {
                self.stack_walk_match_samples.push(format!(
                    "ts={},stack_process={},stack_thread={},frames={}",
                    stack.event_timestamp,
                    stack.stack_process,
                    stack.stack_thread,
                    stack.stack_addresses.len()
                ));
            }
            if !self.attach_stack_walk_event(stack.clone()) {
                self.cache_pending_stack_walk_event(stack);
            }
        }

        fn cache_pending_stack_walk_event(&mut self, stack: DecodedStackWalkEvent) {
            if stack.stack_process != self.target_pid {
                self.dropped_pending_stack_walk_events += 1;
                return;
            }

            if !self
                .pending_stacks_by_timestamp
                .contains_key(&stack.event_timestamp)
                && self.pending_stacks_by_timestamp.len() >= MAX_PENDING_STACK_TIMESTAMPS
            {
                self.drop_oldest_pending_stack_timestamp();
            }

            let pending = self
                .pending_stacks_by_timestamp
                .entry(stack.event_timestamp)
                .or_default();
            if pending.len() >= MAX_PENDING_STACKS_PER_TIMESTAMP {
                self.dropped_pending_stack_walk_events += 1;
                return;
            }
            pending.push(stack);
        }

        fn prune_pending_stacks_before(&mut self, timestamp: i64) {
            while self
                .pending_stacks_by_timestamp
                .first_key_value()
                .map(|(pending_timestamp, _)| *pending_timestamp < timestamp)
                .unwrap_or(false)
            {
                self.drop_oldest_pending_stack_timestamp();
            }
        }

        fn drop_oldest_pending_stack_timestamp(&mut self) {
            let Some(oldest) = self
                .pending_stacks_by_timestamp
                .first_key_value()
                .map(|(timestamp, _)| *timestamp)
            else {
                return;
            };
            if let Some(removed) = self.pending_stacks_by_timestamp.remove(&oldest) {
                self.dropped_pending_stack_walk_events += removed.len() as u64;
            }
        }

        fn attach_stack_walk_event(&mut self, stack: DecodedStackWalkEvent) -> bool {
            let Some(indices) = self
                .event_indices_by_timestamp
                .get(&stack.event_timestamp)
                .cloned()
            else {
                return false;
            };

            let mut fallback_index = None;
            for target in indices {
                let (pid, thread) = self.stack_target_identity(target);
                if pid != self.target_pid {
                    continue;
                }
                fallback_index.get_or_insert(target);
                if thread == stack.stack_thread {
                    self.append_stack_frames_to_target(target, &stack.stack_addresses);
                    self.matched_stack_walk_events += 1;
                    return true;
                }
            }

            if let Some(target) = fallback_index.filter(|_| stack.stack_process == self.target_pid)
            {
                self.append_stack_frames_to_target(target, &stack.stack_addresses);
                self.matched_stack_walk_events += 1;
                return true;
            }

            false
        }

        fn stack_target_identity(&self, target: StackTarget) -> (u32, u32) {
            match target.kind {
                StackTargetKind::Lifecycle => (
                    self.lifecycle_events[target.index].pid,
                    self.lifecycle_header_threads[target.index],
                ),
                StackTargetKind::FileIo => (
                    self.file_io_events[target.index].pid,
                    self.file_io_header_threads[target.index],
                ),
            }
        }

        fn append_stack_frames_to_target(&mut self, target: StackTarget, frames: &[u64]) {
            match target.kind {
                StackTargetKind::Lifecycle => {
                    append_stack_frames(&mut self.lifecycle_stack_addresses[target.index], frames)
                }
                StackTargetKind::FileIo => {
                    append_stack_frames(&mut self.file_io_stack_addresses[target.index], frames)
                }
            }
        }

        fn finalize_stacks(&mut self, stacks_enabled: bool) {
            for (index, addresses) in self.lifecycle_stack_addresses.iter().enumerate() {
                self.lifecycle_events[index].stack = finalize_stack(
                    addresses,
                    &self.modules,
                    self.lifecycle_timestamps[index],
                    &mut self.lifecycle_stack_stats,
                );
            }
            for (index, addresses) in self.file_io_stack_addresses.iter().enumerate() {
                self.file_io_events[index].stack = finalize_stack(
                    addresses,
                    &self.modules,
                    self.file_io_timestamps[index],
                    &mut self.file_io_stack_stats,
                );
            }

            if stacks_enabled && self.stack_walk_events > 0 && self.matched_stack_walk_events == 0 {
                self.warnings.push(format!(
                    "native ETW saw {} StackWalk events but none matched filtered target events; lifecycle_samples=[{}]; stack_samples=[{}]",
                    self.stack_walk_events,
                    self.lifecycle_match_samples.join("; "),
                    self.stack_walk_match_samples.join("; ")
                ));
            }
            if stacks_enabled && self.dropped_pending_stack_walk_events > 0 {
                self.warnings.push(format!(
                    "native ETW dropped {} unmatched StackWalk events while bounding the pending stack cache",
                    self.dropped_pending_stack_walk_events
                ));
            }
        }

        fn finish(mut self) -> EtwSummary {
            if self.file_path_unresolved > 0 {
                self.warnings.push(format!(
                    "native ETW file_io left {} target file events without a resolved path",
                    self.file_path_unresolved
                ));
            }
            if self.unmatched_op_end_count > 0 {
                self.warnings.push(format!(
                    "native ETW file_io saw {} target OpEnd events without a matching begin event",
                    self.unmatched_op_end_count
                ));
            }
            if self.reused_irp_without_op_end_count > 0 {
                self.warnings.push(format!(
                    "native ETW file_io saw {} reused IrpPtr values before a matching OpEnd",
                    self.reused_irp_without_op_end_count
                ));
            }
            let incomplete_io_count =
                self.pending_file_irps.len() as u64 + self.reused_irp_without_op_end_count;
            if incomplete_io_count > 0 {
                self.warnings.push(format!(
                    "native ETW file_io left {} begin events without a matching OpEnd",
                    incomplete_io_count
                ));
            }
            let mut event_sets = BTreeMap::new();
            if self.settings.includes(EtwEventSet::Process) {
                event_sets.insert(
                    "process",
                    EtwEventSetSummary::lifecycle(
                        self.lifecycle_event_counts,
                        self.lifecycle_stack_stats,
                    ),
                );
            }
            if self.settings.includes(EtwEventSet::FileIo) {
                event_sets.insert(
                    "file_io",
                    EtwEventSetSummary::file_io(
                        self.file_io_event_counts,
                        self.file_io_stack_stats,
                        self.file_path_resolved,
                        self.file_path_unresolved,
                        self.matched_op_end_count,
                        self.unmatched_op_end_count,
                        incomplete_io_count,
                        self.reused_irp_without_op_end_count,
                    ),
                );
            }
            EtwSummary {
                target_pid: Some(self.target_pid),
                requested_event_sets: self.settings.event_set_names(),
                stacks_enabled: self.settings.stacks_enabled,
                event_sets,
                warnings: self.warnings,
            }
        }
    }

    fn append_stack_frames(target: &mut Vec<u64>, source: &[u64]) {
        for address in source {
            if *address != 0 && !target.contains(address) {
                target.push(*address);
            }
        }
    }

    fn finalize_stack(
        addresses: &[u64],
        modules: &[ModuleInterval],
        timestamp: i64,
        stats: &mut StackStats,
    ) -> Vec<String> {
        let stack = resolve_stack_addresses(addresses, modules, timestamp);
        stats.total += stack.len() as u64;
        stats.resolved += stack.iter().filter(|frame| frame.resolved).count() as u64;
        stats.unresolved += stack.iter().filter(|frame| !frame.resolved).count() as u64;
        stack.into_iter().rev().map(|frame| frame.value).collect()
    }

    fn lifecycle_count_map() -> BTreeMap<&'static str, u64> {
        [
            ("process_start", 0),
            ("process_end", 0),
            ("thread_start", 0),
            ("thread_end", 0),
            ("image_load", 0),
            ("image_unload", 0),
        ]
        .into_iter()
        .collect()
    }

    fn file_io_count_map() -> BTreeMap<&'static str, u64> {
        [
            ("file_name", 0),
            ("file_create_name", 0),
            ("file_delete_name", 0),
            ("file_rundown", 0),
            ("create", 0),
            ("cleanup", 0),
            ("read", 0),
            ("write", 0),
            ("set_info", 0),
            ("delete", 0),
            ("rename", 0),
            ("dir_enum", 0),
            ("flush", 0),
            ("query_info", 0),
            ("fs_control", 0),
            ("dir_notify", 0),
        ]
        .into_iter()
        .collect()
    }

    fn post_process_trace(
        trace_path: &Path,
        process_path: &Path,
        file_io_path: &Path,
        target_pid: u32,
        settings: NativeEtwSettings,
    ) -> Result<EtwSummary> {
        let mut processor = EtwProcessor::new(target_pid, settings);
        let mut trace_path_w = wide_null(trace_path.as_os_str());
        let mut logfile = EVENT_TRACE_LOGFILEW::default();
        unsafe {
            logfile.LogFileName = PWSTR(trace_path_w.as_mut_ptr());
            logfile.Anonymous1.ProcessTraceMode =
                PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
            logfile.Anonymous2.EventRecordCallback = Some(etw_event_record_callback);
            logfile.Context = (&mut processor as *mut EtwProcessor).cast();

            let handle = OpenTraceW(&mut logfile);
            if handle.Value == u64::MAX {
                return Err(DbgFlowError::Backend(
                    "OpenTraceW failed for native ETW trace".to_string(),
                ));
            }
            let process_status = ProcessTrace(&[handle], None, None);
            let close_status = CloseTrace(handle);
            if process_status != ERROR_SUCCESS {
                return Err(etw_error("ProcessTrace", process_status));
            }
            if close_status != ERROR_SUCCESS {
                processor.warnings.push(format!(
                    "CloseTrace returned Win32 error {}",
                    close_status.0
                ));
            }
        }

        processor.finalize_stacks(settings.stacks_enabled);
        if settings.includes(EtwEventSet::Process) {
            write_events(process_path, &processor.lifecycle_events)?;
        }
        if settings.includes(EtwEventSet::FileIo) {
            write_events(file_io_path, &processor.file_io_events)?;
        }
        Ok(processor.finish())
    }

    unsafe extern "system" fn etw_event_record_callback(record: *mut EVENT_RECORD) {
        if record.is_null() {
            return;
        }
        let processor = (*record).UserContext as *mut EtwProcessor;
        if processor.is_null() {
            return;
        }
        (*processor).process_record(record);
    }

    unsafe fn decode_lifecycle_event(
        record: *mut EVENT_RECORD,
        warnings: &mut Vec<String>,
    ) -> Option<DecodedLifecycleEvent> {
        let header = &(*record).EventHeader;
        let opcode = header.EventDescriptor.Opcode as u32;
        let provider = header.ProviderId;
        let stack_addresses = stack_addresses(record);
        if provider == ProcessGuid {
            let event = match opcode {
                EVENT_TRACE_TYPE_START => "process_start",
                EVENT_TRACE_TYPE_END => "process_end",
                _ => return None,
            };
            let pid = read_u32_any(record, &["ProcessId", "ProcessID", "PID"])?;
            let parent_pid = if event == "process_start" {
                read_u32_any(record, &["ParentId", "ParentID", "ParentProcessId"])
            } else {
                None
            };
            return Some(DecodedLifecycleEvent {
                event_timestamp: header.TimeStamp,
                event_thread_id: header.ThreadId,
                event,
                pid,
                tid: None,
                parent_pid,
                image_path: read_string_any(record, &["ImageFileName", "ImageName"]),
                image_base: None,
                image_size: None,
                stack_addresses,
            });
        }
        if provider == ThreadGuid {
            let event = match opcode {
                EVENT_TRACE_TYPE_START => "thread_start",
                EVENT_TRACE_TYPE_END => "thread_end",
                _ => return None,
            };
            let pid = read_u32_any(record, &["ProcessId", "ProcessID", "PID"])?;
            let tid = read_u32_any(record, &["ThreadId", "ThreadID", "TThreadId", "TThreadID"]);
            return Some(DecodedLifecycleEvent {
                event_timestamp: header.TimeStamp,
                event_thread_id: header.ThreadId,
                event,
                pid,
                tid,
                parent_pid: None,
                image_path: None,
                image_base: None,
                image_size: None,
                stack_addresses,
            });
        }
        if provider == ImageLoadGuid {
            let event = match opcode {
                EVENT_TRACE_TYPE_LOAD => "image_load",
                EVENT_TRACE_TYPE_END => "image_unload",
                _ => return None,
            };
            let Some(pid) = read_u32_any(record, &["ProcessId", "ProcessID", "PID"]) else {
                warnings.push("image event skipped because ProcessId is unavailable".to_string());
                return None;
            };
            return Some(DecodedLifecycleEvent {
                event_timestamp: header.TimeStamp,
                event_thread_id: header.ThreadId,
                event,
                pid,
                tid: None,
                parent_pid: None,
                image_path: read_string_any(record, &["ImageFileName", "FileName", "ImageName"]),
                image_base: read_u64_any(record, &["ImageBase", "BaseAddress"]),
                image_size: read_u64_any(record, &["ImageSize", "Size"]),
                stack_addresses,
            });
        }
        None
    }

    unsafe fn decode_file_io_event(
        record: *mut EVENT_RECORD,
        _warnings: &mut Vec<String>,
    ) -> Option<DecodedFileIoEvent> {
        let header = &(*record).EventHeader;
        if header.ProviderId != FileIoGuid {
            return None;
        }
        let opcode = header.EventDescriptor.Opcode as u32;
        let event = match opcode {
            FILE_IO_NAME => "file_name",
            FILE_IO_FILE_CREATE_NAME => "file_create_name",
            FILE_IO_FILE_DELETE_NAME => "file_delete_name",
            FILE_IO_RUNDOWN => "file_rundown",
            FILE_IO_CREATE => "create",
            FILE_IO_CLEANUP => "cleanup",
            FILE_IO_CLOSE => "close",
            FILE_IO_READ => "read",
            FILE_IO_WRITE => "write",
            FILE_IO_SET_INFO => "set_info",
            FILE_IO_DELETE => "delete",
            FILE_IO_RENAME => "rename",
            FILE_IO_DIR_ENUM => "dir_enum",
            FILE_IO_FLUSH => "flush",
            FILE_IO_QUERY_INFO => "query_info",
            FILE_IO_FS_CONTROL => "fs_control",
            FILE_IO_OP_END => "op_end",
            FILE_IO_DIR_NOTIFY => "dir_notify",
            _ => return None,
        };
        let path = match opcode {
            FILE_IO_CREATE => read_string_any(record, &["OpenPath", "FileName", "Path"]),
            FILE_IO_NAME
            | FILE_IO_FILE_CREATE_NAME
            | FILE_IO_FILE_DELETE_NAME
            | FILE_IO_RUNDOWN
            | FILE_IO_DIR_ENUM
            | FILE_IO_DIR_NOTIFY => read_string_any(record, &["FileName", "OpenPath", "Path"]),
            _ => None,
        };
        let path_source = path.as_ref().map(|_| match opcode {
            FILE_IO_CREATE => "open_path",
            _ => "file_name",
        });
        let stack_addresses = if opcode == FILE_IO_OP_END {
            Vec::new()
        } else {
            stack_addresses(record)
        };
        let pid =
            read_u32_any(record, &["ProcessId", "ProcessID", "PID"]).unwrap_or(header.ProcessId);
        let ttid = read_u32_any(
            record,
            &["TTID", "TThreadId", "TThreadID", "ThreadId", "ThreadID"],
        );

        Some(DecodedFileIoEvent {
            event_timestamp: header.TimeStamp,
            event_thread_id: header.ThreadId,
            event,
            pid,
            ttid,
            path,
            path_source,
            file_object: read_u64_any(record, &["FileObject", "FileObj"]),
            file_key: read_u64_any(record, &["FileKey", "FileObjectKey"]),
            irp_ptr: read_u64_any(record, &["IrpPtr", "Irp", "IrpPointer"]),
            offset: read_u64_any(record, &["Offset", "ByteOffset"]),
            io_size: read_u64_any(record, &["IoSize", "Size", "TransferSize"]),
            io_flags: read_u32_any(record, &["IoFlags", "Flags"]),
            create_options: read_u32_any(record, &["CreateOptions"]),
            file_attributes: read_u32_any(record, &["FileAttributes"]),
            share_access: read_u32_any(record, &["ShareAccess"]),
            info_class: read_u32_any(record, &["InfoClass"]),
            extra_info: read_u64_any(record, &["ExtraInfo"]),
            nt_status: read_u32_any(record, &["NtStatus", "Status"]),
            stack_addresses,
        })
    }

    unsafe fn decode_stack_walk_event(record: *mut EVENT_RECORD) -> Option<DecodedStackWalkEvent> {
        let header = &(*record).EventHeader;
        if header.ProviderId != STACK_WALK_GUID {
            return None;
        }
        if header.EventDescriptor.Opcode as u32 != STACK_WALK_EVENT_TYPE {
            return None;
        }

        if let Some(stack) = decode_stack_walk_event_with_tdh(record) {
            return Some(stack);
        }

        let data = std::slice::from_raw_parts(
            (*record).UserData.cast::<u8>(),
            (*record).UserDataLength as usize,
        );
        decode_stack_walk_user_data(data)
    }

    unsafe fn decode_stack_walk_event_with_tdh(
        record: *const EVENT_RECORD,
    ) -> Option<DecodedStackWalkEvent> {
        let event_timestamp = read_u64_any(record, &["EventTimeStamp"])? as i64;
        let stack_process = read_u32_any(record, &["StackProcess"])?;
        let stack_thread = read_u32_any(record, &["StackThread"])?;
        let mut stack_addresses = Vec::new();
        for index in 1..=192 {
            let property_name = format!("Stack{index}");
            let Some(address) = read_u64_any(record, &[property_name.as_str()]) else {
                break;
            };
            if address != 0 {
                stack_addresses.push(address);
            }
        }

        Some(DecodedStackWalkEvent {
            event_timestamp,
            stack_process,
            stack_thread,
            stack_addresses,
        })
    }

    fn decode_stack_walk_user_data(data: &[u8]) -> Option<DecodedStackWalkEvent> {
        if data.len() < 16 {
            return None;
        }
        let event_timestamp = i64::from_ne_bytes(data[0..8].try_into().ok()?);
        let stack_process = u32::from_ne_bytes(data[8..12].try_into().ok()?);
        let stack_thread = u32::from_ne_bytes(data[12..16].try_into().ok()?);
        let stack_data = &data[16..];
        let pointer_size = if stack_data.len() % size_of::<u64>() == 0 {
            size_of::<u64>()
        } else if stack_data.len() % size_of::<u32>() == 0 {
            size_of::<u32>()
        } else {
            return None;
        };

        let mut stack_addresses = Vec::new();
        for chunk in stack_data.chunks_exact(pointer_size) {
            let address = if pointer_size == size_of::<u64>() {
                u64::from_ne_bytes(chunk.try_into().ok()?)
            } else {
                u32::from_ne_bytes(chunk.try_into().ok()?) as u64
            };
            if address != 0 {
                stack_addresses.push(address);
            }
        }

        Some(DecodedStackWalkEvent {
            event_timestamp,
            stack_process,
            stack_thread,
            stack_addresses,
        })
    }

    unsafe fn stack_addresses(record: *mut EVENT_RECORD) -> Vec<u64> {
        let mut addresses = Vec::new();
        let count = (*record).ExtendedDataCount as usize;
        if count == 0 || (*record).ExtendedData.is_null() {
            return addresses;
        }
        for index in 0..count {
            let item = &*(*record).ExtendedData.add(index);
            match item.ExtType as u32 {
                EVENT_HEADER_EXT_TYPE_STACK_TRACE64 => {
                    if item.DataSize <= 8 || item.DataPtr == 0 {
                        continue;
                    }
                    let frame_count = (item.DataSize as usize - 8) / size_of::<u64>();
                    let ptr = item.DataPtr as *const u8;
                    let frames = ptr.add(8) as *const u64;
                    for frame in std::slice::from_raw_parts(frames, frame_count) {
                        addresses.push(*frame);
                    }
                }
                EVENT_HEADER_EXT_TYPE_STACK_TRACE32 => {
                    if item.DataSize <= 8 || item.DataPtr == 0 {
                        continue;
                    }
                    let frame_count = (item.DataSize as usize - 8) / size_of::<u32>();
                    let ptr = item.DataPtr as *const u8;
                    let frames = ptr.add(8) as *const u32;
                    for frame in std::slice::from_raw_parts(frames, frame_count) {
                        addresses.push(*frame as u64);
                    }
                }
                _ => {}
            }
        }
        addresses
    }

    unsafe fn read_u32_any(record: *const EVENT_RECORD, names: &[&str]) -> Option<u32> {
        names
            .iter()
            .find_map(|name| read_property(record, name).and_then(bytes_to_u32))
    }

    unsafe fn read_u64_any(record: *const EVENT_RECORD, names: &[&str]) -> Option<u64> {
        names
            .iter()
            .find_map(|name| read_property(record, name).and_then(bytes_to_u64))
    }

    unsafe fn read_string_any(record: *const EVENT_RECORD, names: &[&str]) -> Option<String> {
        names
            .iter()
            .find_map(|name| read_string_property(record, name))
    }

    unsafe fn read_string_property(record: *const EVENT_RECORD, name: &str) -> Option<String> {
        let bytes = read_property(record, name)?;
        let encoding = property_string_encoding(record, name).unwrap_or(StringEncoding::Unknown);
        bytes_to_string(bytes, encoding)
    }

    unsafe fn read_property(record: *const EVENT_RECORD, name: &str) -> Option<Vec<u8>> {
        let name_w = wide_null(OsStr::new(name));
        let descriptor = PROPERTY_DATA_DESCRIPTOR {
            PropertyName: name_w.as_ptr() as u64,
            ArrayIndex: u32::MAX,
            Reserved: 0,
        };
        let mut size = 0u32;
        let size_status = windows::Win32::System::Diagnostics::Etw::TdhGetPropertySize(
            record,
            None,
            &[descriptor],
            &mut size,
        );
        if size_status != ERROR_SUCCESS.0 || size == 0 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        let status = TdhGetProperty(record, None, &[descriptor], &mut bytes);
        if status != ERROR_SUCCESS.0 {
            return None;
        }
        Some(bytes)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StringEncoding {
        Unicode,
        Ansi,
        Unknown,
    }

    unsafe fn property_string_encoding(
        record: *const EVENT_RECORD,
        name: &str,
    ) -> Option<StringEncoding> {
        let in_type = property_in_type(record, name)?;
        if is_unicode_string_in_type(in_type) {
            Some(StringEncoding::Unicode)
        } else if is_ansi_string_in_type(in_type) {
            Some(StringEncoding::Ansi)
        } else {
            None
        }
    }

    unsafe fn property_in_type(record: *const EVENT_RECORD, name: &str) -> Option<u16> {
        let mut size = 0u32;
        let size_status = TdhGetEventInformation(record, None, None, &mut size);
        if size_status != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
            return None;
        }

        let mut buffer = vec![0u8; size as usize];
        let info = buffer.as_mut_ptr().cast::<TRACE_EVENT_INFO>();
        let status = TdhGetEventInformation(record, None, Some(info), &mut size);
        if status != ERROR_SUCCESS.0 {
            return None;
        }

        let info = &*info;
        let properties = std::slice::from_raw_parts(
            info.EventPropertyInfoArray.as_ptr(),
            info.PropertyCount as usize,
        );
        for property in properties {
            if property.Flags.0 & PropertyStruct.0 != 0 {
                continue;
            }
            let Some(property_name) = trace_event_info_string(&buffer, property.NameOffset) else {
                continue;
            };
            if property_name == name {
                return Some(property.Anonymous1.nonStructType.InType);
            }
        }

        None
    }

    fn trace_event_info_string(buffer: &[u8], offset: u32) -> Option<String> {
        let offset = offset as usize;
        if offset == 0 || offset >= buffer.len() {
            return None;
        }
        utf16_null_terminated_to_string(&buffer[offset..])
    }

    fn is_unicode_string_in_type(in_type: u16) -> bool {
        let in_type = in_type as i32;
        matches!(
            in_type,
            value if value == TDH_INTYPE_UNICODESTRING.0
                || value == TDH_INTYPE_COUNTEDSTRING.0
                || value == TDH_INTYPE_MANIFEST_COUNTEDSTRING.0
                || value == TDH_INTYPE_NONNULLTERMINATEDSTRING.0
                || value == TDH_INTYPE_REVERSEDCOUNTEDSTRING.0
        )
    }

    fn is_ansi_string_in_type(in_type: u16) -> bool {
        let in_type = in_type as i32;
        matches!(
            in_type,
            value if value == TDH_INTYPE_ANSISTRING.0
                || value == TDH_INTYPE_COUNTEDANSISTRING.0
                || value == TDH_INTYPE_MANIFEST_COUNTEDANSISTRING.0
                || value == TDH_INTYPE_NONNULLTERMINATEDANSISTRING.0
                || value == TDH_INTYPE_REVERSEDCOUNTEDANSISTRING.0
        )
    }

    fn bytes_to_u32(bytes: Vec<u8>) -> Option<u32> {
        if bytes.len() < size_of::<u32>() {
            return None;
        }
        let raw: [u8; 4] = bytes[..4].try_into().ok()?;
        Some(u32::from_ne_bytes(raw))
    }

    fn bytes_to_u64(bytes: Vec<u8>) -> Option<u64> {
        if bytes.len() >= size_of::<u64>() {
            let raw: [u8; 8] = bytes[..8].try_into().ok()?;
            return Some(u64::from_ne_bytes(raw));
        }
        if bytes.len() >= size_of::<u32>() {
            return bytes_to_u32(bytes).map(u64::from);
        }
        None
    }

    fn bytes_to_string(bytes: Vec<u8>, encoding: StringEncoding) -> Option<String> {
        if bytes.is_empty() {
            return None;
        }
        match encoding {
            StringEncoding::Unicode => return utf16_null_terminated_to_string(&bytes),
            StringEncoding::Ansi => return ansi_null_terminated_to_string(&bytes),
            StringEncoding::Unknown => {}
        }
        if looks_like_utf16le(&bytes) {
            return utf16_null_terminated_to_string(&bytes);
        }
        ansi_null_terminated_to_string(&bytes)
    }

    fn utf16_null_terminated_to_string(bytes: &[u8]) -> Option<String> {
        let words = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_ne_bytes([chunk[0], chunk[1]]))
            .take_while(|word| *word != 0)
            .collect::<Vec<_>>();
        if words.is_empty() {
            None
        } else {
            Some(String::from_utf16_lossy(&words))
        }
    }

    fn ansi_null_terminated_to_string(bytes: &[u8]) -> Option<String> {
        let nul = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if nul == 0 {
            None
        } else {
            Some(String::from_utf8_lossy(&bytes[..nul]).to_string())
        }
    }

    fn looks_like_utf16le(bytes: &[u8]) -> bool {
        if bytes.len() % 2 != 0 {
            return false;
        }
        if bytes.chunks_exact(2).any(|chunk| chunk == [0, 0]) {
            return true;
        }
        let mut words = 0usize;
        let mut zero_high_bytes = 0usize;
        for chunk in bytes.chunks_exact(2) {
            words += 1;
            if chunk[1] == 0 {
                zero_high_bytes += 1;
            }
        }
        words > 0 && zero_high_bytes * 2 >= words
    }

    fn write_events<T: Serialize>(path: &Path, events: &[T]) -> Result<()> {
        let mut file =
            File::create(path).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        for event in events {
            let line = serde_json::to_string(event)
                .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
            writeln!(file, "{line}").map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        }
        Ok(())
    }

    fn ensure_empty_event_set_files(
        settings: NativeEtwSettings,
        process_path: &Path,
        file_io_path: &Path,
    ) -> Result<()> {
        if settings.includes(EtwEventSet::Process) {
            ensure_empty_file(process_path)?;
        }
        if settings.includes(EtwEventSet::FileIo) {
            ensure_empty_file(file_io_path)?;
        }
        Ok(())
    }

    fn ensure_empty_file(path: &Path) -> Result<()> {
        File::create(path)
            .map(|_| ())
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))
    }

    fn collector_artifacts(
        settings: NativeEtwSettings,
        trace_path: &Path,
        process_path: &Path,
        file_io_path: &Path,
        summary_path: &Path,
    ) -> Vec<ArtifactRef> {
        let mut artifacts = vec![ArtifactRef {
            kind: ArtifactKind::ProfileCollectorTrace,
            path: trace_path.to_path_buf(),
        }];
        if settings.includes(EtwEventSet::Process) {
            artifacts.push(ArtifactRef {
                kind: ArtifactKind::ProfileCollectorEvents,
                path: process_path.to_path_buf(),
            });
        }
        if settings.includes(EtwEventSet::FileIo) {
            artifacts.push(ArtifactRef {
                kind: ArtifactKind::ProfileCollectorEvents,
                path: file_io_path.to_path_buf(),
            });
        }
        artifacts.push(ArtifactRef {
            kind: ArtifactKind::ProfileCollectorSummary,
            path: summary_path.to_path_buf(),
        });
        artifacts
    }

    fn write_summary(path: &Path, summary: &EtwSummary) -> Result<()> {
        let text = serde_json::to_string_pretty(&json!(summary))
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        std::fs::write(path, text).map_err(|error| DbgFlowError::Artifact(error.to_string()))
    }

    struct EtwPropertiesBuffer {
        ptr: NonNull<u8>,
        layout: Layout,
    }

    impl EtwPropertiesBuffer {
        fn new(size: usize) -> Result<Self> {
            let layout = Layout::from_size_align(size, align_of::<EVENT_TRACE_PROPERTIES>())
                .map_err(|error| {
                    DbgFlowError::Backend(format!("invalid ETW buffer layout: {error}"))
                })?;
            let ptr = unsafe { alloc_zeroed(layout) };
            let ptr = NonNull::new(ptr).ok_or_else(|| {
                DbgFlowError::Backend("allocate ETW properties buffer".to_string())
            })?;
            Ok(Self { ptr, layout })
        }

        fn properties(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
            self.ptr.as_ptr() as *mut EVENT_TRACE_PROPERTIES
        }

        unsafe fn as_bytes_mut(&mut self) -> &mut [u8] {
            std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size())
        }
    }

    impl Drop for EtwPropertiesBuffer {
        fn drop(&mut self) {
            unsafe {
                dealloc(self.ptr.as_ptr(), self.layout);
            }
        }
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    unsafe fn copy_wide_to_buffer(buffer: &mut [u8], byte_offset: usize, value: &[u16]) {
        let destination = buffer.as_mut_ptr().add(byte_offset) as *mut u16;
        std::ptr::copy_nonoverlapping(value.as_ptr(), destination, value.len());
    }

    fn etw_error(operation: &str, status: WIN32_ERROR) -> DbgFlowError {
        DbgFlowError::Backend(format!("{operation} failed with Win32 error {}", status.0))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn test_settings() -> NativeEtwSettings {
            NativeEtwSettings {
                event_sets: [Some(EtwEventSet::Process), Some(EtwEventSet::FileIo)],
                stacks_enabled: true,
            }
        }

        #[test]
        fn stack_walk_after_lifecycle_event_attaches_by_timestamp_and_thread() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_lifecycle_event(DecodedLifecycleEvent {
                event_timestamp: 100,
                event_thread_id: 7,
                event: "image_load",
                pid: 42,
                tid: None,
                parent_pid: None,
                image_path: Some("target.dll".to_string()),
                image_base: Some(0x1000),
                image_size: Some(0x1000),
                stack_addresses: Vec::new(),
            });
            processor.process_stack_walk_event(DecodedStackWalkEvent {
                event_timestamp: 100,
                stack_process: 42,
                stack_thread: 7,
                stack_addresses: vec![0x1010, 0x5000],
            });

            processor.finalize_stacks(true);

            assert_eq!(processor.lifecycle_events[0].event_set, "process");
            assert_eq!(processor.lifecycle_events[0].stack.len(), 2);
            assert_eq!(
                processor.lifecycle_events[0].stack[0].as_str(),
                "0x0000000000005000"
            );
            assert_eq!(
                processor.lifecycle_events[0].stack[1].as_str(),
                "target.dll+0x10"
            );
            assert_eq!(processor.lifecycle_stack_stats.total, 2);
            assert_eq!(processor.lifecycle_stack_stats.resolved, 1);
            assert_eq!(processor.lifecycle_stack_stats.unresolved, 1);
        }

        #[test]
        fn process_summary_and_artifact_use_new_event_set_name() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_lifecycle_event(DecodedLifecycleEvent {
                event_timestamp: 100,
                event_thread_id: 7,
                event: "process_start",
                pid: 42,
                tid: None,
                parent_pid: Some(1),
                image_path: Some("target.exe".to_string()),
                image_base: None,
                image_size: None,
                stack_addresses: Vec::new(),
            });

            let summary = processor.finish();

            assert!(summary.event_sets.contains_key("process"));
            assert_eq!(
                summary.event_sets["process"].event_counts["process_start"],
                1
            );

            let artifacts = collector_artifacts(
                test_settings(),
                Path::new(r"C:\trace.etl"),
                Path::new(r"C:\process.jsonl"),
                Path::new(r"C:\file_io.jsonl"),
                Path::new(r"C:\summary.json"),
            );

            assert!(artifacts
                .iter()
                .any(|artifact| artifact.path.ends_with("process.jsonl")));
            assert!(!artifacts
                .iter()
                .any(|artifact| artifact.path.ends_with("process_lifecycle.jsonl")));
        }

        #[test]
        fn stack_walk_before_lifecycle_event_is_attached_when_event_arrives() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.modules.push(ModuleInterval {
                base: 0x2000,
                size: 0x1000,
                name: "later.dll".to_string(),
                loaded_at: 100,
                unloaded_at: None,
            });
            processor.process_stack_walk_event(DecodedStackWalkEvent {
                event_timestamp: 200,
                stack_process: 42,
                stack_thread: 9,
                stack_addresses: vec![0x2020],
            });
            processor.process_lifecycle_event(DecodedLifecycleEvent {
                event_timestamp: 200,
                event_thread_id: 9,
                event: "thread_start",
                pid: 42,
                tid: Some(123),
                parent_pid: None,
                image_path: None,
                image_base: None,
                image_size: None,
                stack_addresses: Vec::new(),
            });

            processor.finalize_stacks(true);

            assert_eq!(
                processor.lifecycle_events[0].stack[0].as_str(),
                "later.dll+0x20"
            );
            assert_eq!(processor.matched_stack_walk_events, 1);
        }

        #[test]
        fn non_target_stack_walk_without_lifecycle_match_is_not_cached() {
            let mut processor = EtwProcessor::new(42, test_settings());

            processor.process_stack_walk_event(DecodedStackWalkEvent {
                event_timestamp: 300,
                stack_process: 7,
                stack_thread: 9,
                stack_addresses: vec![0x2020],
            });

            assert!(processor.pending_stacks_by_timestamp.is_empty());
            assert_eq!(processor.dropped_pending_stack_walk_events, 1);
        }

        #[test]
        fn file_io_event_resolves_path_from_prior_name_event() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 10,
                event_thread_id: 7,
                event: "file_name",
                pid: 42,
                ttid: None,
                path: Some(r"\Device\HarddiskVolume1\data.txt".to_string()),
                path_source: Some("file_name"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: None,
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "read",
                pid: 42,
                ttid: Some(8),
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: Some(0),
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            assert_eq!(processor.file_io_events.len(), 2);
            assert_eq!(
                processor.file_io_events[1].path.as_deref(),
                Some(r"\Device\HarddiskVolume1\data.txt")
            );
            assert_eq!(
                processor.file_io_events[1].path_source,
                Some("file_object_cache")
            );
            assert_eq!(processor.file_path_resolved, 2);
            assert_eq!(processor.file_path_unresolved, 0);
        }

        #[test]
        fn file_io_path_cache_ignores_non_target_name_events() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 10,
                event_thread_id: 7,
                event: "file_name",
                pid: 7,
                ttid: None,
                path: Some(r"\Device\HarddiskVolume1\other-process.txt".to_string()),
                path_source: Some("file_name"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: None,
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "read",
                pid: 42,
                ttid: Some(8),
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: Some(0),
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            assert_eq!(processor.file_io_events.len(), 1);
            assert_eq!(processor.file_io_events[0].path, None);
            assert_eq!(processor.file_path_unresolved, 1);
        }

        #[test]
        fn file_io_close_is_ignored_and_removes_path_cache() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 10,
                event_thread_id: 7,
                event: "file_name",
                pid: 42,
                ttid: None,
                path: Some(r"\Device\HarddiskVolume1\data.txt".to_string()),
                path_source: Some("file_name"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: None,
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });
            assert_eq!(processor.file_io_events.len(), 1);
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 7,
                event: "close",
                pid: 42,
                ttid: None,
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 25,
                event_thread_id: 7,
                event: "op_end",
                pid: 42,
                ttid: None,
                path: None,
                path_source: None,
                file_object: None,
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: Some(0),
                nt_status: Some(0),
                stack_addresses: Vec::new(),
            });

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 30,
                event_thread_id: 8,
                event: "read",
                pid: 42,
                ttid: Some(8),
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x3000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: Some(0),
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            assert_eq!(processor.file_io_events.len(), 2);
            assert_eq!(processor.file_io_events[1].event, "read");
            assert_eq!(processor.file_io_events[1].path, None);
            assert_eq!(processor.unmatched_op_end_count, 0);
            assert!(!processor.file_io_event_counts.contains_key("close"));
        }

        #[test]
        fn file_io_event_keeps_unresolved_pointer_when_path_is_unknown() {
            let mut processor = EtwProcessor::new(42, test_settings());

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "write",
                pid: 42,
                ttid: Some(8),
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: Some(0x3000),
                irp_ptr: Some(0x2000),
                offset: Some(64),
                io_size: Some(256),
                io_flags: Some(1),
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            assert_eq!(processor.file_io_events[0].path, None);
            assert_eq!(
                processor.file_io_events[0].file_object.as_deref(),
                Some("0x0000000000001000")
            );
            assert_eq!(processor.file_path_resolved, 0);
            assert_eq!(processor.file_path_unresolved, 1);
        }

        #[test]
        fn file_io_op_end_enriches_begin_event_without_path_resolution_pressure() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 10,
                event_thread_id: 8,
                event: "read",
                pid: 42,
                ttid: Some(8),
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "op_end",
                pid: 4,
                ttid: None,
                path: None,
                path_source: None,
                file_object: None,
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: Some(128),
                nt_status: Some(0),
                stack_addresses: Vec::new(),
            });

            assert_eq!(processor.file_io_events.len(), 1);
            assert_eq!(processor.file_io_events[0].event, "read");
            assert_eq!(
                processor.file_io_events[0].extra_info.as_deref(),
                Some("0x0000000000000080")
            );
            assert_eq!(
                processor.file_io_events[0].nt_status.as_deref(),
                Some("0x00000000")
            );
            assert_eq!(processor.file_io_events[0].completion_pid, Some(4));
            assert_eq!(processor.file_io_events[0].completion_tid, Some(8));
            assert_eq!(processor.file_io_events[0].completion_sequence, Some(2));
            assert_eq!(processor.matched_op_end_count, 1);
            assert_eq!(processor.unmatched_op_end_count, 0);
            assert!(processor.pending_file_irps.is_empty());
            assert_eq!(processor.file_path_unresolved, 1);
            assert!(!processor.file_io_event_counts.contains_key("op_end"));
        }

        #[test]
        fn file_io_unmatched_op_end_is_counted_without_output() {
            let mut processor = EtwProcessor::new(42, test_settings());

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "op_end",
                pid: 42,
                ttid: None,
                path: None,
                path_source: None,
                file_object: None,
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: Some(128),
                nt_status: Some(0),
                stack_addresses: Vec::new(),
            });

            let summary = processor.finish();

            assert_eq!(
                summary.event_sets["file_io"].unmatched_op_end_count,
                Some(1)
            );
            assert_eq!(summary.event_sets["file_io"].matched_op_end_count, Some(0));
            assert!(summary
                .warnings
                .iter()
                .any(|warning| warning.contains("without a matching begin event")));
        }

        #[test]
        fn file_io_begin_without_completion_is_counted() {
            let mut processor = EtwProcessor::new(42, test_settings());

            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "write",
                pid: 42,
                ttid: Some(8),
                path: Some(r"C:\data.txt".to_string()),
                path_source: Some("open_path"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            let summary = processor.finish();

            assert_eq!(summary.event_sets["file_io"].incomplete_io_count, Some(1));
            assert!(summary
                .warnings
                .iter()
                .any(|warning| warning.contains("without a matching OpEnd")));
        }

        #[test]
        fn file_io_reused_irp_warns_and_matches_latest_begin() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 10,
                event_thread_id: 7,
                event: "read",
                pid: 42,
                ttid: Some(7),
                path: Some(r"C:\old.txt".to_string()),
                path_source: Some("open_path"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(64),
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "write",
                pid: 42,
                ttid: Some(8),
                path: Some(r"C:\new.txt".to_string()),
                path_source: Some("open_path"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 30,
                event_thread_id: 9,
                event: "op_end",
                pid: 4,
                ttid: None,
                path: None,
                path_source: None,
                file_object: None,
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: None,
                io_size: None,
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: Some(128),
                nt_status: Some(0),
                stack_addresses: Vec::new(),
            });

            assert_eq!(processor.file_io_events.len(), 2);
            assert_eq!(processor.file_io_events[0].completion_sequence, None);
            assert_eq!(processor.file_io_events[1].completion_sequence, Some(3));

            let summary = processor.finish();

            assert_eq!(
                summary.event_sets["file_io"].reused_irp_without_op_end_count,
                Some(1)
            );
            assert_eq!(summary.event_sets["file_io"].matched_op_end_count, Some(1));
            assert_eq!(summary.event_sets["file_io"].incomplete_io_count, Some(1));
            assert!(summary
                .warnings
                .iter()
                .any(|warning| warning.contains("reused IrpPtr")));
        }

        #[test]
        fn summary_reports_counts_by_event_set() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "read",
                pid: 42,
                ttid: Some(8),
                path: Some(r"C:\data.txt".to_string()),
                path_source: Some("open_path"),
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: vec![0x5000],
            });
            processor.finalize_stacks(true);

            let summary = processor.finish();

            assert_eq!(summary.event_sets["file_io"].event_counts["read"], 1);
            assert_eq!(summary.event_sets["file_io"].file_path_resolved, Some(1));
            assert_eq!(summary.event_sets["file_io"].stack_frames_total, 1);
        }

        #[test]
        fn summary_warns_when_file_paths_are_unresolved() {
            let mut processor = EtwProcessor::new(42, test_settings());
            processor.process_file_io_event(DecodedFileIoEvent {
                event_timestamp: 20,
                event_thread_id: 8,
                event: "write",
                pid: 42,
                ttid: Some(8),
                path: None,
                path_source: None,
                file_object: Some(0x1000),
                file_key: None,
                irp_ptr: Some(0x2000),
                offset: Some(0),
                io_size: Some(128),
                io_flags: None,
                create_options: None,
                file_attributes: None,
                share_access: None,
                info_class: None,
                extra_info: None,
                nt_status: None,
                stack_addresses: Vec::new(),
            });

            let summary = processor.finish();

            assert!(summary
                .warnings
                .iter()
                .any(|warning| warning.contains("without a resolved path")));
        }

        #[test]
        fn decodes_stack_walk_user_data_as_64_bit_frames() {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&123_i64.to_ne_bytes());
            bytes.extend_from_slice(&42_u32.to_ne_bytes());
            bytes.extend_from_slice(&7_u32.to_ne_bytes());
            bytes.extend_from_slice(&0x1000_u64.to_ne_bytes());
            bytes.extend_from_slice(&0x2000_u64.to_ne_bytes());

            let stack = decode_stack_walk_user_data(&bytes).expect("decode stack walk data");

            assert_eq!(stack.event_timestamp, 123);
            assert_eq!(stack.stack_process, 42);
            assert_eq!(stack.stack_thread, 7);
            assert_eq!(stack.stack_addresses, vec![0x1000, 0x2000]);
        }

        #[test]
        fn decodes_ansi_process_image_name_without_utf16_misdetect() {
            let text = bytes_to_string(b"cmd.exe\0".to_vec(), StringEncoding::Unknown)
                .expect("decode ansi string");

            assert_eq!(text, "cmd.exe");
        }

        #[test]
        fn decodes_utf16le_image_path() {
            let mut bytes = "cmd.exe"
                .encode_utf16()
                .flat_map(|word| word.to_ne_bytes())
                .collect::<Vec<_>>();
            bytes.extend_from_slice(&0_u16.to_ne_bytes());

            let text =
                bytes_to_string(bytes, StringEncoding::Unknown).expect("decode utf16 string");

            assert_eq!(text, "cmd.exe");
        }

        #[test]
        fn decodes_non_ascii_utf16le_image_path_without_ascii_heuristic() {
            let mut bytes = "模块.dll"
                .encode_utf16()
                .flat_map(|word| word.to_ne_bytes())
                .collect::<Vec<_>>();
            bytes.extend_from_slice(&0_u16.to_ne_bytes());

            let text =
                bytes_to_string(bytes, StringEncoding::Unicode).expect("decode unicode string");

            assert_eq!(text, "模块.dll");
        }
    }
}

#[cfg(windows)]
pub use windows_impl::NativeEtwCollectorFactory;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_stack_frame_to_compact_stack_string() {
        let modules = vec![ModuleInterval {
            base: 0x1000,
            size: 0x200,
            name: "app.exe".to_string(),
            loaded_at: 10,
            unloaded_at: None,
        }];

        let frames = resolve_stack_addresses(&[0x1010], &modules, 10);

        assert_eq!(frames[0].value, "app.exe+0x10");
        assert!(frames[0].resolved);
    }

    #[test]
    fn leaves_unknown_stack_frame_unresolved() {
        let frames = resolve_stack_addresses(&[0x5000], &[], 10);

        assert_eq!(frames[0].value, "0x0000000000005000");
        assert!(!frames[0].resolved);
    }

    #[test]
    fn resolves_stack_frame_using_module_active_at_event_timestamp() {
        let modules = vec![
            ModuleInterval {
                base: 0x1000,
                size: 0x200,
                name: "old.dll".to_string(),
                loaded_at: 10,
                unloaded_at: Some(20),
            },
            ModuleInterval {
                base: 0x1000,
                size: 0x200,
                name: "new.dll".to_string(),
                loaded_at: 30,
                unloaded_at: None,
            },
        ];

        let old_frame = resolve_stack_addresses(&[0x1010], &modules, 15);
        let gap_frame = resolve_stack_addresses(&[0x1010], &modules, 25);
        let new_frame = resolve_stack_addresses(&[0x1010], &modules, 35);

        assert_eq!(old_frame[0].value, "old.dll+0x10");
        assert!(old_frame[0].resolved);
        assert_eq!(gap_frame[0].value, "0x0000000000001010");
        assert!(!gap_frame[0].resolved);
        assert_eq!(new_frame[0].value, "new.dll+0x10");
        assert!(new_frame[0].resolved);
    }

    #[test]
    fn filters_lifecycle_events_to_target_pid() {
        assert!(event_matches_target(1234, 1234));
        assert!(!event_matches_target(4321, 1234));
    }

    #[test]
    fn native_etw_settings_rejects_empty_event_sets() {
        let error = native_etw_settings(&ProfileCollectorConfig::NativeEtw {
            scope: EtwProfileScope::TargetProcess,
            event_sets: Vec::new(),
            stacks: crate::profile::EtwStackConfig::default(),
        })
        .expect_err("empty event sets are rejected");

        assert!(error.to_string().contains("at least one event set"));
    }

    #[test]
    fn native_etw_settings_rejects_duplicate_event_sets() {
        let error = native_etw_settings(&ProfileCollectorConfig::NativeEtw {
            scope: EtwProfileScope::TargetProcess,
            event_sets: vec![EtwEventSet::FileIo, EtwEventSet::FileIo],
            stacks: crate::profile::EtwStackConfig::default(),
        })
        .expect_err("duplicate event sets are rejected");

        assert!(error.to_string().contains("duplicate native ETW event set"));
    }
}
