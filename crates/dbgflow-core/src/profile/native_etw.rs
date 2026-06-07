#[cfg(not(windows))]
use super::{
    CollectorFactory, ProfileCollector, ProfileCollectorConfig, ProfileCollectorKind,
    ProfilePreset,
};
#[cfg(not(windows))]
use crate::{DbgFlowError, Result};
#[cfg(not(windows))]
use std::path::Path;

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

#[cfg(not(windows))]
impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        _trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        if config.kind != ProfileCollectorKind::NativeEtw
            || config.preset != ProfilePreset::SystemOverview
        {
            return Err(DbgFlowError::Backend(
                "unsupported native ETW profile collector configuration".to_string(),
            ));
        }
        Err(DbgFlowError::Backend(
            "native ETW profiling is only supported on Windows".to_string(),
        ))
    }
}

#[cfg(windows)]
use super::{
    CollectorFactory, CollectorStart, CollectorStop, ProfileCollector, ProfileCollectorConfig,
    ProfileCollectorKind, ProfilePreset,
};
#[cfg(windows)]
use crate::{DbgFlowError, Result};
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::mem::size_of;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use uuid::Uuid;
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::Etw::{
    ControlTraceW, StartTraceW, CONTROLTRACE_HANDLE, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_FLAG_CSWITCH, EVENT_TRACE_FLAG_DISK_FILE_IO,
    EVENT_TRACE_FLAG_DISK_IO, EVENT_TRACE_FLAG_IMAGE_LOAD, EVENT_TRACE_FLAG_PROCESS,
    EVENT_TRACE_FLAG_PROFILE, EVENT_TRACE_FLAG_REGISTRY, EVENT_TRACE_FLAG_THREAD,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_SYSTEM_LOGGER_MODE, WNODE_FLAG_TRACED_GUID,
};

#[cfg(windows)]
#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

#[cfg(windows)]
impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        if config.kind != ProfileCollectorKind::NativeEtw
            || config.preset != ProfilePreset::SystemOverview
        {
            return Err(DbgFlowError::Backend(
                "unsupported native ETW profile collector configuration".to_string(),
            ));
        }
        Ok(Box::new(NativeEtwCollector::new(trace_path.to_path_buf())))
    }
}

#[cfg(windows)]
struct NativeEtwCollector {
    trace_path: PathBuf,
    state: Mutex<NativeEtwState>,
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct NativeEtwState {
    session_name: Option<String>,
}

#[cfg(windows)]
impl NativeEtwCollector {
    fn new(trace_path: PathBuf) -> Self {
        Self {
            trace_path,
            state: Mutex::new(NativeEtwState::default()),
        }
    }
}

#[cfg(windows)]
impl ProfileCollector for NativeEtwCollector {
    fn start(&self, _output_dir: &Path) -> Result<CollectorStart> {
        let mut state = self.state.lock().map_err(|_| {
            DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
        })?;
        if state.session_name.is_some() {
            return Err(DbgFlowError::Backend(
                "native ETW collector already started".to_string(),
            ));
        }

        let session_name = format!("dbgflow-profile-{}", Uuid::new_v4());
        start_trace_session(&session_name, &self.trace_path)?;
        state.session_name = Some(session_name);
        Ok(CollectorStart {
            warnings: Vec::new(),
        })
    }

    fn stop(&self) -> Result<CollectorStop> {
        let mut state = self.state.lock().map_err(|_| {
            DbgFlowError::Backend("native ETW collector lock poisoned".to_string())
        })?;
        let Some(session_name) = state.session_name.take() else {
            return Ok(CollectorStop {
                warnings: vec!["native ETW collector was not started".to_string()],
            });
        };

        stop_trace_session(&session_name)?;
        Ok(CollectorStop {
            warnings: Vec::new(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        let session_name = self
            .state
            .lock()
            .map_err(|_| DbgFlowError::Backend("native ETW collector lock poisoned".to_string()))?
            .session_name
            .clone();
        if let Some(session_name) = session_name {
            let _ = stop_trace_session(&session_name);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn start_trace_session(session_name: &str, trace_path: &Path) -> Result<()> {
    let session_name_w = wide_null(OsStr::new(session_name));
    let trace_path_w = wide_null(trace_path.as_os_str());
    let properties_size = size_of::<EVENT_TRACE_PROPERTIES>()
        + session_name_w.len() * size_of::<u16>()
        + trace_path_w.len() * size_of::<u16>();
    let mut buffer = vec![0u8; properties_size];
    let properties = buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;

    unsafe {
        (*properties).Wnode.BufferSize = properties_size as u32;
        (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*properties).Wnode.ClientContext = 1;
        (*properties).LogFileMode = EVENT_TRACE_FILE_MODE_SEQUENTIAL
            | EVENT_TRACE_SYSTEM_LOGGER_MODE;
        (*properties).EnableFlags = EVENT_TRACE_FLAG_PROCESS
            | EVENT_TRACE_FLAG_THREAD
            | EVENT_TRACE_FLAG_IMAGE_LOAD
            | EVENT_TRACE_FLAG_PROFILE
            | EVENT_TRACE_FLAG_CSWITCH
            | EVENT_TRACE_FLAG_DISK_IO
            | EVENT_TRACE_FLAG_DISK_FILE_IO
            | EVENT_TRACE_FLAG_REGISTRY;
        (*properties).BufferSize = 1024;
        (*properties).MinimumBuffers = 64;
        (*properties).MaximumBuffers = 256;
        (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        (*properties).LogFileNameOffset =
            (size_of::<EVENT_TRACE_PROPERTIES>() + session_name_w.len() * size_of::<u16>()) as u32;

        copy_wide_to_buffer(&mut buffer, (*properties).LoggerNameOffset as usize, &session_name_w);
        copy_wide_to_buffer(&mut buffer, (*properties).LogFileNameOffset as usize, &trace_path_w);

        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        let status = StartTraceW(&mut handle, PCWSTR(session_name_w.as_ptr()), properties);
        if status != ERROR_SUCCESS {
            return Err(etw_error("StartTraceW", status));
        }
    }

    Ok(())
}

#[cfg(windows)]
fn stop_trace_session(session_name: &str) -> Result<()> {
    let session_name_w = wide_null(OsStr::new(session_name));
    let properties_size =
        size_of::<EVENT_TRACE_PROPERTIES>() + session_name_w.len() * size_of::<u16>();
    let mut buffer = vec![0u8; properties_size];
    let properties = buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;

    unsafe {
        (*properties).Wnode.BufferSize = properties_size as u32;
        (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        copy_wide_to_buffer(&mut buffer, (*properties).LoggerNameOffset as usize, &session_name_w);

        let status = ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            PCWSTR(session_name_w.as_ptr()),
            properties,
            EVENT_TRACE_CONTROL_STOP,
        );
        if status != ERROR_SUCCESS {
            return Err(etw_error("ControlTraceW stop", status));
        }
    }

    Ok(())
}

#[cfg(windows)]
fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
unsafe fn copy_wide_to_buffer(buffer: &mut [u8], byte_offset: usize, value: &[u16]) {
    let destination = buffer.as_mut_ptr().add(byte_offset) as *mut u16;
    std::ptr::copy_nonoverlapping(value.as_ptr(), destination, value.len());
}

#[cfg(windows)]
fn etw_error(operation: &str, status: WIN32_ERROR) -> DbgFlowError {
    DbgFlowError::Backend(format!(
        "{operation} failed with Win32 error {}",
        status.0
    ))
}
