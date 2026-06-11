use super::{
    CollectorFactory, EtwEventSet, EtwProfileScope, ProfileCollector, ProfileCollectorConfig,
    ProfileCollectorKind,
};
use crate::{DbgFlowError, Result};
#[cfg(not(windows))]
use std::path::Path;

#[derive(Debug, Clone, Copy)]
struct NativeEtwSettings {
    stacks_enabled: bool,
}

fn native_etw_settings(config: &ProfileCollectorConfig) -> Result<NativeEtwSettings> {
    let ProfileCollectorConfig::NativeEtw {
        scope,
        event_sets,
        stacks,
    } = config
    else {
        return Err(DbgFlowError::Backend(
            "unsupported native ETW profile collector configuration".to_string(),
        ));
    };
    if scope != &EtwProfileScope::TargetProcess {
        return Err(DbgFlowError::Backend(
            "native ETW scope must be target_process".to_string(),
        ));
    }
    if event_sets.as_slice() != [EtwEventSet::ProcessLifecycle] {
        return Err(DbgFlowError::Backend(
            "native ETW event_sets must be [process_lifecycle]".to_string(),
        ));
    }
    Ok(NativeEtwSettings {
        stacks_enabled: stacks.enabled,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StackFrame {
    value: String,
    resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModuleInterval {
    base: u64,
    size: u64,
    name: String,
    loaded_at: i64,
    unloaded_at: Option<i64>,
}

impl ModuleInterval {
    fn contains_at(&self, address: u64, timestamp: i64) -> bool {
        address >= self.base
            && address < self.base.saturating_add(self.size)
            && timestamp >= self.loaded_at
            && self
                .unloaded_at
                .map(|unloaded_at| timestamp <= unloaded_at)
                .unwrap_or(true)
    }
}

fn resolve_stack_addresses(
    addresses: &[u64],
    modules: &[ModuleInterval],
    timestamp: i64,
) -> Vec<StackFrame> {
    addresses
        .iter()
        .map(|address| resolve_stack_address(*address, modules, timestamp))
        .collect()
}

fn resolve_stack_address(address: u64, modules: &[ModuleInterval], timestamp: i64) -> StackFrame {
    if let Some(module) = modules
        .iter()
        .filter(|module| module.contains_at(address, timestamp))
        .max_by_key(|module| module.loaded_at)
    {
        let offset = address - module.base;
        return StackFrame {
            value: format!("{}+0x{offset:x}", module.name),
            resolved: true,
        };
    }
    StackFrame {
        value: hex64(address),
        resolved: false,
    }
}

fn event_matches_target(pid: u32, target_pid: u32) -> bool {
    pid == target_pid
}

fn hex64(value: u64) -> String {
    format!("0x{value:016x}")
}

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
    use crate::artifacts::{ArtifactKind, ArtifactRef};
    use serde::Serialize;
    use serde_json::json;
    use std::alloc::{alloc_zeroed, dealloc, Layout};
    use std::collections::BTreeMap;
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
        CloseTrace, ControlTraceW, ImageLoadGuid, OpenTraceW, ProcessGuid, ProcessTrace,
        PropertyStruct, StartTraceW, TdhGetEventInformation, TdhGetProperty, ThreadGuid,
        TraceSetInformation, TraceStackTracingInfo, CLASSIC_EVENT_ID, CONTROLTRACE_HANDLE,
        EVENT_HEADER_EXT_TYPE_STACK_TRACE32, EVENT_HEADER_EXT_TYPE_STACK_TRACE64, EVENT_RECORD,
        EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_FLAG_IMAGE_LOAD,
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
                output_dir.join("events.jsonl"),
                output_dir.join("summary.json"),
                settings,
            )))
        }
    }

    struct NativeEtwCollector {
        trace_path: PathBuf,
        events_path: PathBuf,
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
            events_path: PathBuf,
            summary_path: PathBuf,
            settings: NativeEtwSettings,
        ) -> Self {
            Self {
                trace_path,
                events_path,
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
            start_trace_session(
                &session_name,
                &self.trace_path,
                self.settings.stacks_enabled,
            )?;
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
                        &self.events_path,
                        pid,
                        self.settings.stacks_enabled,
                    ) {
                        Ok(summary) => summary,
                        Err(error) => {
                            warnings.push(format!("native ETW post-processing failed: {error}"));
                            if let Err(error) = ensure_empty_file(&self.events_path) {
                                warnings.push(format!(
                                    "native ETW events fallback write failed: {error}"
                                ));
                            }
                            EtwSummary::empty(
                                Some(pid),
                                self.settings.stacks_enabled,
                                warnings.clone(),
                            )
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
                    if let Err(error) = ensure_empty_file(&self.events_path) {
                        warnings.push(format!("native ETW events fallback write failed: {error}"));
                    }
                    let summary =
                        EtwSummary::empty(None, self.settings.stacks_enabled, warnings.clone());
                    if let Err(error) = write_summary(&self.summary_path, &summary) {
                        warnings.push(format!("native ETW summary write failed: {error}"));
                    }
                }
            }

            Ok(super::super::CollectorStop {
                artifacts: vec![
                    ArtifactRef {
                        kind: ArtifactKind::ProfileCollectorTrace,
                        path: self.trace_path.clone(),
                    },
                    ArtifactRef {
                        kind: ArtifactKind::ProfileCollectorEvents,
                        path: self.events_path.clone(),
                    },
                    ArtifactRef {
                        kind: ArtifactKind::ProfileCollectorSummary,
                        path: self.summary_path.clone(),
                    },
                ],
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
        stacks_enabled: bool,
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
            (*properties).EnableFlags =
                EVENT_TRACE_FLAG_PROCESS | EVENT_TRACE_FLAG_THREAD | EVENT_TRACE_FLAG_IMAGE_LOAD;
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
            if stacks_enabled {
                if let Err(error) = enable_process_lifecycle_stacks(handle) {
                    let _ = stop_trace_session(session_name, trace_path);
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    unsafe fn enable_process_lifecycle_stacks(handle: CONTROLTRACE_HANDLE) -> Result<()> {
        let events = [
            classic_event(ProcessGuid, EVENT_TRACE_TYPE_START),
            classic_event(ProcessGuid, EVENT_TRACE_TYPE_END),
            classic_event(ThreadGuid, EVENT_TRACE_TYPE_START),
            classic_event(ThreadGuid, EVENT_TRACE_TYPE_END),
            classic_event(ImageLoadGuid, EVENT_TRACE_TYPE_LOAD),
            classic_event(ImageLoadGuid, EVENT_TRACE_TYPE_END),
        ];
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
        event_sets: Vec<&'static str>,
        stacks_enabled: bool,
        event_counts: BTreeMap<&'static str, u64>,
        stack_frames_total: u64,
        stack_frames_resolved: u64,
        stack_frames_unresolved: u64,
        warnings: Vec<String>,
    }

    impl EtwSummary {
        fn empty(target_pid: Option<u32>, stacks_enabled: bool, warnings: Vec<String>) -> Self {
            Self {
                target_pid,
                event_sets: vec!["process_lifecycle"],
                stacks_enabled,
                event_counts: lifecycle_count_map(),
                stack_frames_total: 0,
                stack_frames_resolved: 0,
                stack_frames_unresolved: 0,
                warnings,
            }
        }
    }

    #[derive(Debug, Serialize)]
    struct LifecycleEvent {
        sequence: u64,
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

    #[derive(Debug, Clone)]
    struct DecodedStackWalkEvent {
        event_timestamp: i64,
        stack_process: u32,
        stack_thread: u32,
        stack_addresses: Vec<u64>,
    }

    struct EtwProcessor {
        target_pid: u32,
        events: Vec<LifecycleEvent>,
        event_timestamps: Vec<i64>,
        event_header_threads: Vec<u32>,
        event_stack_addresses: Vec<Vec<u64>>,
        event_indices_by_timestamp: BTreeMap<i64, Vec<usize>>,
        pending_stacks_by_timestamp: BTreeMap<i64, Vec<DecodedStackWalkEvent>>,
        modules: Vec<ModuleInterval>,
        warnings: Vec<String>,
        event_counts: BTreeMap<&'static str, u64>,
        stack_frames_total: u64,
        stack_frames_resolved: u64,
        stack_frames_unresolved: u64,
        stack_walk_events: u64,
        matched_stack_walk_events: u64,
        dropped_pending_stack_walk_events: u64,
        lifecycle_match_samples: Vec<String>,
        stack_walk_match_samples: Vec<String>,
    }

    impl EtwProcessor {
        fn new(target_pid: u32) -> Self {
            Self {
                target_pid,
                events: Vec::new(),
                event_timestamps: Vec::new(),
                event_header_threads: Vec::new(),
                event_stack_addresses: Vec::new(),
                event_indices_by_timestamp: BTreeMap::new(),
                pending_stacks_by_timestamp: BTreeMap::new(),
                modules: Vec::new(),
                warnings: Vec::new(),
                event_counts: lifecycle_count_map(),
                stack_frames_total: 0,
                stack_frames_resolved: 0,
                stack_frames_unresolved: 0,
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
            let Some(decoded) = decode_lifecycle_event(record, &mut self.warnings) else {
                return;
            };
            self.process_lifecycle_event(decoded);
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

            if let Some(count) = self.event_counts.get_mut(decoded.event) {
                *count += 1;
            }
            if self.lifecycle_match_samples.len() < 5 {
                self.lifecycle_match_samples.push(format!(
                    "{}:ts={},header_tid={},pid={}",
                    decoded.event, decoded.event_timestamp, decoded.event_thread_id, decoded.pid
                ));
            }
            let sequence = self.events.len() as u64 + 1;
            let event_index = self.events.len();
            self.events.push(LifecycleEvent {
                sequence,
                event: decoded.event,
                pid: decoded.pid,
                tid: decoded.tid,
                parent_pid: decoded.parent_pid,
                image_path: decoded.image_path.clone(),
                image_base: decoded.image_base.map(hex64),
                image_size: decoded.image_size.map(hex64),
                stack: Vec::new(),
            });
            self.event_timestamps.push(decoded.event_timestamp);
            self.event_header_threads.push(decoded.event_thread_id);
            self.event_stack_addresses
                .push(decoded.stack_addresses.clone());
            self.event_indices_by_timestamp
                .entry(decoded.event_timestamp)
                .or_default()
                .push(event_index);

            if let Some(pending) = self
                .pending_stacks_by_timestamp
                .remove(&decoded.event_timestamp)
            {
                for stack in pending {
                    self.attach_stack_walk_event(stack);
                }
            }
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
            for index in indices {
                if self.events[index].pid != self.target_pid {
                    continue;
                }
                fallback_index.get_or_insert(index);
                if self.event_header_threads[index] == stack.stack_thread {
                    append_stack_frames(
                        &mut self.event_stack_addresses[index],
                        &stack.stack_addresses,
                    );
                    self.matched_stack_walk_events += 1;
                    return true;
                }
            }

            if let Some(index) = fallback_index.filter(|_| stack.stack_process == self.target_pid) {
                append_stack_frames(
                    &mut self.event_stack_addresses[index],
                    &stack.stack_addresses,
                );
                self.matched_stack_walk_events += 1;
                return true;
            }

            false
        }

        fn finalize_stacks(&mut self, stacks_enabled: bool) {
            for (index, addresses) in self.event_stack_addresses.iter().enumerate() {
                let stack =
                    resolve_stack_addresses(addresses, &self.modules, self.event_timestamps[index]);
                self.stack_frames_total += stack.len() as u64;
                self.stack_frames_resolved +=
                    stack.iter().filter(|frame| frame.resolved).count() as u64;
                self.stack_frames_unresolved +=
                    stack.iter().filter(|frame| !frame.resolved).count() as u64;
                self.events[index].stack = stack.into_iter().map(|frame| frame.value).collect();
            }

            if stacks_enabled && self.stack_walk_events > 0 && self.matched_stack_walk_events == 0 {
                self.warnings.push(format!(
                    "native ETW saw {} StackWalk events but none matched filtered lifecycle events; lifecycle_samples=[{}]; stack_samples=[{}]",
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

        fn finish(self, stacks_enabled: bool) -> EtwSummary {
            EtwSummary {
                target_pid: Some(self.target_pid),
                event_sets: vec!["process_lifecycle"],
                stacks_enabled,
                event_counts: self.event_counts,
                stack_frames_total: self.stack_frames_total,
                stack_frames_resolved: self.stack_frames_resolved,
                stack_frames_unresolved: self.stack_frames_unresolved,
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

    fn post_process_trace(
        trace_path: &Path,
        events_path: &Path,
        target_pid: u32,
        stacks_enabled: bool,
    ) -> Result<EtwSummary> {
        let mut processor = EtwProcessor::new(target_pid);
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

        processor.finalize_stacks(stacks_enabled);
        write_events(events_path, &processor.events)?;
        Ok(processor.finish(stacks_enabled))
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

    fn write_events(path: &Path, events: &[LifecycleEvent]) -> Result<()> {
        let mut file =
            File::create(path).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        for event in events {
            let line = serde_json::to_string(event)
                .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
            writeln!(file, "{line}").map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        }
        Ok(())
    }

    fn ensure_empty_file(path: &Path) -> Result<()> {
        File::create(path)
            .map(|_| ())
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))
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

        #[test]
        fn stack_walk_after_lifecycle_event_attaches_by_timestamp_and_thread() {
            let mut processor = EtwProcessor::new(42);
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

            assert_eq!(processor.events[0].stack.len(), 2);
            assert_eq!(processor.events[0].stack[0].as_str(), "target.dll+0x10");
            assert_eq!(processor.events[0].stack[1].as_str(), "0x0000000000005000");
            assert_eq!(processor.stack_frames_total, 2);
            assert_eq!(processor.stack_frames_resolved, 1);
            assert_eq!(processor.stack_frames_unresolved, 1);
        }

        #[test]
        fn stack_walk_before_lifecycle_event_is_attached_when_event_arrives() {
            let mut processor = EtwProcessor::new(42);
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

            assert_eq!(processor.events[0].stack[0].as_str(), "later.dll+0x20");
            assert_eq!(processor.matched_stack_walk_events, 1);
        }

        #[test]
        fn non_target_stack_walk_without_lifecycle_match_is_not_cached() {
            let mut processor = EtwProcessor::new(42);

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

        #[test]
        #[ignore = "requires DBGFLOW_ETL_REPLAY_TRACE and DBGFLOW_ETL_REPLAY_TARGET_PID"]
        fn postprocess_existing_etl_from_env_produces_stacks() {
            let trace = std::env::var_os("DBGFLOW_ETL_REPLAY_TRACE")
                .map(PathBuf::from)
                .expect("DBGFLOW_ETL_REPLAY_TRACE");
            let target_pid = std::env::var("DBGFLOW_ETL_REPLAY_TARGET_PID")
                .expect("DBGFLOW_ETL_REPLAY_TARGET_PID")
                .parse::<u32>()
                .expect("target pid");
            let output_dir = std::env::temp_dir().join(format!(
                "dbgflow-etl-replay-{}-{}",
                std::process::id(),
                target_pid
            ));
            let _ = std::fs::remove_dir_all(&output_dir);
            std::fs::create_dir_all(&output_dir).expect("create replay output dir");
            let events_path = output_dir.join("events.jsonl");

            let summary =
                post_process_trace(&trace, &events_path, target_pid, true).expect("post process");
            let events = std::fs::read_to_string(&events_path).expect("read events");

            assert!(
                summary.stack_frames_total > 0,
                "expected stack frames from ETL replay; summary={}",
                serde_json::to_string_pretty(&json!(summary)).expect("summary json")
            );
            assert!(
                events.contains(r#""stack":["#),
                "expected at least one serialized stack frame"
            );
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
}
