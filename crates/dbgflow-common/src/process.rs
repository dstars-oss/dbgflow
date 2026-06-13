use crate::logging::{LogEvent, LogLevel, LogSink};
use crate::{DbgFlowError, Result};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildIdentity {
    CurrentProcess,
    McpPeerSession,
    ActiveInteractiveSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackChildIdentity {
    CurrentProcess,
    ActiveInteractiveSession,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessLaunchConfig {
    pub child_identity: ChildIdentity,
    pub fallback_child_identity: FallbackChildIdentity,
    pub elevate_if_admin: bool,
}

impl Default for ProcessLaunchConfig {
    fn default() -> Self {
        Self {
            child_identity: ChildIdentity::CurrentProcess,
            fallback_child_identity: FallbackChildIdentity::CurrentProcess,
            elevate_if_admin: false,
        }
    }
}

impl ProcessLaunchConfig {
    pub fn installed_service_default() -> Self {
        Self {
            child_identity: ChildIdentity::McpPeerSession,
            fallback_child_identity: FallbackChildIdentity::ActiveInteractiveSession,
            elevate_if_admin: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallContext {
    pub peer_pid: Option<u32>,
    pub peer_session_id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessLaunchContext {
    pub config: ProcessLaunchConfig,
    pub tool_call: ToolCallContext,
}

impl Default for ProcessLaunchContext {
    fn default() -> Self {
        Self {
            config: ProcessLaunchConfig::default(),
            tool_call: ToolCallContext::default(),
        }
    }
}

impl ProcessLaunchContext {
    pub fn new(config: ProcessLaunchConfig, tool_call: ToolCallContext) -> Self {
        Self { config, tool_call }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessIdentitySource {
    CurrentProcess,
    McpPeerSession,
    ActiveInteractiveSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessLaunchAudit {
    pub requested_identity: ChildIdentity,
    pub resolved_source: ProcessIdentitySource,
    pub peer_pid: Option<u32>,
    pub peer_session_id: Option<u32>,
    pub target_session_id: Option<u32>,
    pub elevated: bool,
    pub fallback_reason: Option<String>,
}

impl ProcessLaunchAudit {
    fn current_process(context: &ProcessLaunchContext, fallback_reason: Option<String>) -> Self {
        Self {
            requested_identity: context.config.child_identity,
            resolved_source: ProcessIdentitySource::CurrentProcess,
            peer_pid: context.tool_call.peer_pid,
            peer_session_id: context.tool_call.peer_session_id,
            target_session_id: None,
            elevated: false,
            fallback_reason,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchStdio {
    Null,
    Piped,
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvChange {
    pub key: OsString,
    pub value: Option<OsString>,
}

impl EnvChange {
    pub fn set(key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        Self {
            key: key.into(),
            value: Some(value.into()),
        }
    }

    pub fn remove(key: impl Into<OsString>) -> Self {
        Self {
            key: key.into(),
            value: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessLaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<EnvChange>,
    pub stdin: LaunchStdio,
    pub stdout: LaunchStdio,
    pub stderr: LaunchStdio,
    pub creation_flags: u32,
    pub current_dir: Option<PathBuf>,
}

impl ProcessLaunchSpec {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: LaunchStdio::Null,
            stdout: LaunchStdio::Null,
            stderr: LaunchStdio::Null,
            creation_flags: 0,
            current_dir: None,
        }
    }

    pub fn hide_console_window(&mut self) {
        #[cfg(windows)]
        {
            self.creation_flags |= windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
}

pub struct ManagedChild {
    pid: u32,
    #[cfg(windows)]
    process: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    thread: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    child: std::process::Child,
    stdin: Option<File>,
    stdout: Option<File>,
    audit: ProcessLaunchAudit,
}

impl ManagedChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn audit(&self) -> &ProcessLaunchAudit {
        &self.audit
    }

    pub fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    pub fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
        #[cfg(windows)]
        {
            windows_try_wait(self.process)
        }
        #[cfg(not(windows))]
        {
            self.child
                .try_wait()
                .map(|status| {
                    status.map(|status| ProcessExit {
                        code: status.code(),
                    })
                })
                .map_err(|error| DbgFlowError::Backend(format!("poll process failed: {error}")))
        }
    }

    pub fn wait(&mut self) -> Result<ProcessExit> {
        #[cfg(windows)]
        {
            windows_wait(self.process)
        }
        #[cfg(not(windows))]
        {
            self.child
                .wait()
                .map(|status| ProcessExit {
                    code: status.code(),
                })
                .map_err(|error| DbgFlowError::Backend(format!("wait process failed: {error}")))
        }
    }

    pub fn kill(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            windows_terminate(self.process)
        }
        #[cfg(not(windows))]
        {
            self.child
                .kill()
                .map_err(|error| DbgFlowError::Backend(format!("kill process failed: {error}")))
        }
    }

    #[cfg(windows)]
    pub fn raw_process_handle(&self) -> isize {
        self.process
    }

    #[cfg(windows)]
    pub fn raw_thread_handle(&self) -> isize {
        self.thread
    }

    #[cfg(windows)]
    pub fn resume_thread(&self) -> Result<()> {
        let previous = unsafe { windows_sys::Win32::System::Threading::ResumeThread(self.thread) };
        if previous == u32::MAX {
            return Err(DbgFlowError::Backend("ResumeThread failed".to_string()));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ManagedChild {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.thread);
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.process);
        }
    }
}

pub fn log_process_launch(
    logger: &Arc<dyn LogSink>,
    component: &'static str,
    event: &'static str,
    audit: &ProcessLaunchAudit,
) {
    logger.log(
        LogEvent::new(LogLevel::Info, component, event)
            .field(
                "requested_identity",
                format!("{:?}", audit.requested_identity),
            )
            .field("resolved_source", format!("{:?}", audit.resolved_source))
            .field("peer_pid", audit.peer_pid)
            .field("peer_session_id", audit.peer_session_id)
            .field("target_session_id", audit.target_session_id)
            .field("elevated", audit.elevated)
            .field("fallback_reason", audit.fallback_reason.clone()),
    );
}

pub fn spawn_process(
    spec: &ProcessLaunchSpec,
    context: &ProcessLaunchContext,
) -> Result<ManagedChild> {
    #[cfg(windows)]
    {
        windows_spawn_process(spec, context)
    }
    #[cfg(not(windows))]
    {
        std_spawn_process(spec, context)
    }
}

pub fn resolve_peer_process_context(local: SocketAddr, peer: SocketAddr) -> ToolCallContext {
    #[cfg(windows)]
    {
        let peer_pid = windows_tcp_owner_pid(local, peer).ok();
        let peer_session_id = peer_pid.and_then(|pid| windows_process_session_id(pid).ok());
        ToolCallContext {
            peer_pid,
            peer_session_id,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (local, peer);
        ToolCallContext::default()
    }
}

#[cfg(not(windows))]
fn std_spawn_process(
    spec: &ProcessLaunchSpec,
    context: &ProcessLaunchContext,
) -> Result<ManagedChild> {
    if spec.creation_flags != 0 {
        return Err(DbgFlowError::Backend(
            "process creation flags are only supported on Windows".to_string(),
        ));
    }
    let mut command = Command::new(&spec.executable);
    command.args(&spec.args);
    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }
    for change in &spec.env {
        match &change.value {
            Some(value) => command.env(&change.key, value),
            None => command.env_remove(&change.key),
        };
    }
    apply_stdio(&mut command, &spec.stdin, true)?;
    apply_stdio(&mut command, &spec.stdout, false)?;
    apply_stdio(&mut command, &spec.stderr, false)?;
    let mut child = command.spawn().map_err(|error| {
        DbgFlowError::Backend(format!(
            "spawn process {} failed: {error}",
            spec.executable.display()
        ))
    })?;
    let stdin = child.stdin.take().map(child_stdio_to_file);
    let stdout = child.stdout.take().map(child_stdio_to_file);
    let pid = child.id();
    Ok(ManagedChild {
        pid,
        child,
        stdin,
        stdout,
        audit: ProcessLaunchAudit::current_process(context, None),
    })
}

#[cfg(not(windows))]
fn apply_stdio(command: &mut Command, stdio: &LaunchStdio, input: bool) -> Result<()> {
    match stdio {
        LaunchStdio::Null => {
            if input {
                command.stdin(Stdio::null());
            } else {
                command.stdout(Stdio::null());
            }
        }
        LaunchStdio::Piped => {
            if input {
                command.stdin(Stdio::piped());
            } else {
                command.stdout(Stdio::piped());
            }
        }
        LaunchStdio::File(path) => {
            let file = if input {
                File::open(path)
            } else {
                File::create(path)
            }
            .map_err(|error| {
                DbgFlowError::Artifact(format!("open stdio file {}: {error}", path.display()))
            })?;
            if input {
                command.stdin(Stdio::from(file));
            } else {
                command.stdout(Stdio::from(file));
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn child_stdio_to_file<T>(_stdio: T) -> File {
    unreachable!("non-Windows tests do not use managed stdio files")
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::collections::BTreeMap;
    use std::mem::{size_of, zeroed};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, SetHandleInformation, DUPLICATE_SAME_ACCESS,
        ERROR_INSUFFICIENT_BUFFER, HANDLE, HANDLE_FLAG_INHERIT, NO_ERROR, WAIT_FAILED,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_ALL,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenLinkedToken, SECURITY_ATTRIBUTES, TOKEN_LINKED_TOKEN,
    };
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
        GetExitCodeProcess, InitializeProcThreadAttributeList, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    };

    const STILL_ACTIVE_EXIT: u32 = 259;
    const DETACHED_SESSION: u32 = u32::MAX;

    pub(super) fn windows_spawn_process(
        spec: &ProcessLaunchSpec,
        context: &ProcessLaunchContext,
    ) -> Result<ManagedChild> {
        match resolve_user_identity(context) {
            Ok(ResolvedIdentity::Token(resolved)) => {
                match create_process_with_optional_token(spec, Some(&resolved)) {
                    Ok(mut child) => {
                        child.audit = resolved.audit;
                        Ok(child)
                    }
                    Err(error) => {
                        let reason = format!(
                            "CreateProcessAsUserW failed; falling back to current_process: {error}"
                        );
                        create_process_with_optional_token(spec, None).map(|mut child| {
                            child.audit =
                                ProcessLaunchAudit::current_process(context, Some(reason));
                            child
                        })
                    }
                }
            }
            Ok(ResolvedIdentity::CurrentProcess { fallback_reason }) => {
                create_process_with_optional_token(spec, None).map(|mut child| {
                    child.audit = ProcessLaunchAudit::current_process(context, fallback_reason);
                    child
                })
            }
            Err(error) => {
                let reason = format!("{error}; falling back to current_process");
                create_process_with_optional_token(spec, None).map(|mut child| {
                    child.audit = ProcessLaunchAudit::current_process(context, Some(reason));
                    child
                })
            }
        }
    }

    struct ResolvedToken {
        token: HandleGuard,
        audit: ProcessLaunchAudit,
    }

    enum ResolvedIdentity {
        Token(ResolvedToken),
        CurrentProcess { fallback_reason: Option<String> },
    }

    fn resolve_user_identity(context: &ProcessLaunchContext) -> Result<ResolvedIdentity> {
        match context.config.child_identity {
            ChildIdentity::CurrentProcess => Ok(ResolvedIdentity::CurrentProcess {
                fallback_reason: None,
            }),
            ChildIdentity::McpPeerSession => {
                if let Some(session_id) = context.tool_call.peer_session_id {
                    return match token_for_session(
                        context,
                        ProcessIdentitySource::McpPeerSession,
                        session_id,
                        None,
                    ) {
                        Ok(token) => Ok(ResolvedIdentity::Token(token)),
                        Err(error) => resolve_fallback_identity(
                            context,
                            format!("MCP peer session token could not be opened: {error}"),
                        ),
                    };
                }
                resolve_fallback_identity(context, "MCP peer session was not resolved".to_string())
            }
            ChildIdentity::ActiveInteractiveSession => match active_console_session_id() {
                Some(session_id) => match token_for_session(
                    context,
                    ProcessIdentitySource::ActiveInteractiveSession,
                    session_id,
                    None,
                ) {
                    Ok(token) => Ok(ResolvedIdentity::Token(token)),
                    Err(error) => resolve_fallback_identity(
                        context,
                        format!("active interactive session token could not be opened: {error}"),
                    ),
                },
                None => resolve_fallback_identity(
                    context,
                    "active interactive session was not available".to_string(),
                ),
            },
        }
    }

    fn resolve_fallback_identity(
        context: &ProcessLaunchContext,
        reason: String,
    ) -> Result<ResolvedIdentity> {
        match context.config.fallback_child_identity {
            FallbackChildIdentity::CurrentProcess => Ok(ResolvedIdentity::CurrentProcess {
                fallback_reason: Some(reason),
            }),
            FallbackChildIdentity::ActiveInteractiveSession => {
                let Some(session_id) = active_console_session_id() else {
                    return Ok(ResolvedIdentity::CurrentProcess {
                        fallback_reason: Some(format!(
                            "{reason}; active interactive session was not available"
                        )),
                    });
                };
                match token_for_session(
                    context,
                    ProcessIdentitySource::ActiveInteractiveSession,
                    session_id,
                    Some(reason.clone()),
                ) {
                    Ok(token) => Ok(ResolvedIdentity::Token(token)),
                    Err(error) => Ok(ResolvedIdentity::CurrentProcess {
                        fallback_reason: Some(format!(
                            "{reason}; active interactive session token could not be opened: {error}"
                        )),
                    }),
                }
            }
        }
    }

    fn token_for_session(
        context: &ProcessLaunchContext,
        source: ProcessIdentitySource,
        session_id: u32,
        fallback_reason: Option<String>,
    ) -> Result<ResolvedToken> {
        let mut token: HANDLE = 0;
        let ok = unsafe { WTSQueryUserToken(session_id, &mut token) };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "WTSQueryUserToken({session_id}) failed: {}",
                last_error()
            )));
        }
        let mut token = HandleGuard(token);
        let mut elevated = false;
        if context.config.elevate_if_admin {
            if let Some(linked) = linked_token(token.0) {
                token = linked;
                elevated = true;
            }
        }
        Ok(ResolvedToken {
            token,
            audit: ProcessLaunchAudit {
                requested_identity: context.config.child_identity,
                resolved_source: source,
                peer_pid: context.tool_call.peer_pid,
                peer_session_id: context.tool_call.peer_session_id,
                target_session_id: Some(session_id),
                elevated,
                fallback_reason,
            },
        })
    }

    fn active_console_session_id() -> Option<u32> {
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        (session_id != DETACHED_SESSION).then_some(session_id)
    }

    fn linked_token(token: HANDLE) -> Option<HandleGuard> {
        let mut linked = TOKEN_LINKED_TOKEN { LinkedToken: 0 };
        let mut returned = 0;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenLinkedToken,
                &mut linked as *mut _ as *mut _,
                size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut returned,
            )
        };
        (ok != 0 && linked.LinkedToken != 0).then_some(HandleGuard(linked.LinkedToken))
    }

    struct PreparedStdio {
        child_stdin: HandleGuard,
        child_stdout: HandleGuard,
        child_stderr: HandleGuard,
        parent_stdin: Option<File>,
        parent_stdout: Option<File>,
        _files: Vec<File>,
    }

    fn prepare_stdio(spec: &ProcessLaunchSpec) -> Result<PreparedStdio> {
        let (child_stdin, parent_stdin, stdin_file) = prepare_input(&spec.stdin)?;
        let (child_stdout, parent_stdout, stdout_file) = prepare_output(&spec.stdout)?;
        let (child_stderr, _parent_stderr, stderr_file) = prepare_output(&spec.stderr)?;
        let mut files = Vec::new();
        files.extend(stdin_file);
        files.extend(stdout_file);
        files.extend(stderr_file);
        Ok(PreparedStdio {
            child_stdin,
            child_stdout,
            child_stderr,
            parent_stdin,
            parent_stdout,
            _files: files,
        })
    }

    fn prepare_input(stdio: &LaunchStdio) -> Result<(HandleGuard, Option<File>, Option<File>)> {
        match stdio {
            LaunchStdio::Piped => {
                let (read, mut write) = create_pipe_pair()?;
                set_handle_not_inheritable(write.0)?;
                let parent = unsafe { File::from_raw_handle(write.take() as *mut _) };
                Ok((read, Some(parent), None))
            }
            LaunchStdio::File(path) => {
                let file = File::open(path).map_err(|error| {
                    DbgFlowError::Artifact(format!("open stdin file {}: {error}", path.display()))
                })?;
                Ok((
                    duplicate_inheritable(file.as_raw_handle() as HANDLE)?,
                    None,
                    Some(file),
                ))
            }
            LaunchStdio::Null => {
                let file = File::open("NUL").map_err(|error| {
                    DbgFlowError::Artifact(format!("open NUL for stdin: {error}"))
                })?;
                Ok((
                    duplicate_inheritable(file.as_raw_handle() as HANDLE)?,
                    None,
                    Some(file),
                ))
            }
        }
    }

    fn prepare_output(stdio: &LaunchStdio) -> Result<(HandleGuard, Option<File>, Option<File>)> {
        match stdio {
            LaunchStdio::Piped => {
                let (mut read, write) = create_pipe_pair()?;
                set_handle_not_inheritable(read.0)?;
                let parent = unsafe { File::from_raw_handle(read.take() as *mut _) };
                Ok((write, Some(parent), None))
            }
            LaunchStdio::File(path) => {
                let file = File::create(path).map_err(|error| {
                    DbgFlowError::Artifact(format!(
                        "create stdout file {}: {error}",
                        path.display()
                    ))
                })?;
                Ok((
                    duplicate_inheritable(file.as_raw_handle() as HANDLE)?,
                    None,
                    Some(file),
                ))
            }
            LaunchStdio::Null => {
                let file = File::create("NUL").map_err(|error| {
                    DbgFlowError::Artifact(format!("open NUL for stdout: {error}"))
                })?;
                Ok((
                    duplicate_inheritable(file.as_raw_handle() as HANDLE)?,
                    None,
                    Some(file),
                ))
            }
        }
    }

    fn create_pipe_pair() -> Result<(HandleGuard, HandleGuard)> {
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let mut read: HANDLE = 0;
        let mut write: HANDLE = 0;
        let ok = unsafe { CreatePipe(&mut read, &mut write, &mut attrs, 0) };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "CreatePipe failed: {}",
                last_error()
            )));
        }
        Ok((HandleGuard(read), HandleGuard(write)))
    }

    fn duplicate_inheritable(handle: HANDLE) -> Result<HandleGuard> {
        let mut duplicate = 0;
        let current = unsafe { GetCurrentProcess() };
        let ok = unsafe {
            DuplicateHandle(
                current,
                handle,
                current,
                &mut duplicate,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "DuplicateHandle failed: {}",
                last_error()
            )));
        }
        Ok(HandleGuard(duplicate))
    }

    fn set_handle_not_inheritable(handle: HANDLE) -> Result<()> {
        let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "SetHandleInformation failed: {}",
                last_error()
            )));
        }
        Ok(())
    }

    fn create_process_with_optional_token(
        spec: &ProcessLaunchSpec,
        token: Option<&ResolvedToken>,
    ) -> Result<ManagedChild> {
        let mut stdio = prepare_stdio(spec)?;
        let mut executable = to_wide_null(&spec.executable);
        let command_line = command_line(&spec.executable, &spec.args);
        let mut command_line = to_wide_null_str(&command_line);
        let current_dir_wide = spec
            .current_dir
            .as_ref()
            .map(|path| to_wide_null(path.as_path()));
        let current_dir_ptr = current_dir_wide
            .as_ref()
            .map(|value| value.as_ptr())
            .unwrap_or(std::ptr::null());
        let mut startup_info: STARTUPINFOEXW = unsafe { zeroed() };
        startup_info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup_info.StartupInfo.hStdInput = stdio.child_stdin.0;
        startup_info.StartupInfo.hStdOutput = stdio.child_stdout.0;
        startup_info.StartupInfo.hStdError = stdio.child_stderr.0;
        let mut inheritable_handles = [
            stdio.child_stdin.0,
            stdio.child_stdout.0,
            stdio.child_stderr.0,
        ];
        let mut attribute_list = AttributeList::new(&mut inheritable_handles)?;
        startup_info.lpAttributeList = attribute_list.as_ptr();
        let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };
        let env = match token {
            Some(token) => Some(EnvironmentBlock::for_user(token.token.0, &spec.env)?),
            None if spec.env.is_empty() => None,
            None => Some(EnvironmentBlock::current(&spec.env)?),
        };
        let env_ptr = env
            .as_ref()
            .map(|env| env.as_ptr())
            .unwrap_or(std::ptr::null());
        let creation_flags = spec.creation_flags
            | env
                .as_ref()
                .map(|_| CREATE_UNICODE_ENVIRONMENT)
                .unwrap_or(0)
            | EXTENDED_STARTUPINFO_PRESENT;
        let startup_info_ptr = &mut startup_info as *mut STARTUPINFOEXW as *mut STARTUPINFOW;

        let ok = unsafe {
            match token {
                Some(token) => CreateProcessAsUserW(
                    token.token.0,
                    executable.as_mut_ptr(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    creation_flags,
                    env_ptr,
                    current_dir_ptr,
                    startup_info_ptr,
                    &mut process_info,
                ),
                None => CreateProcessW(
                    executable.as_mut_ptr(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    creation_flags,
                    env_ptr,
                    current_dir_ptr,
                    startup_info_ptr,
                    &mut process_info,
                ),
            }
        };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "spawn process {} failed: {}",
                spec.executable.display(),
                last_error()
            )));
        }

        Ok(ManagedChild {
            pid: process_info.dwProcessId,
            process: process_info.hProcess,
            thread: process_info.hThread,
            stdin: stdio.parent_stdin.take(),
            stdout: stdio.parent_stdout.take(),
            audit: ProcessLaunchAudit {
                requested_identity: ChildIdentity::CurrentProcess,
                resolved_source: ProcessIdentitySource::CurrentProcess,
                peer_pid: None,
                peer_session_id: None,
                target_session_id: None,
                elevated: false,
                fallback_reason: None,
            },
        })
    }

    struct EnvironmentBlock {
        data: Vec<u16>,
    }

    impl EnvironmentBlock {
        fn for_user(token: HANDLE, env_changes: &[EnvChange]) -> Result<Self> {
            let mut raw = std::ptr::null_mut();
            let ok = unsafe { CreateEnvironmentBlock(&mut raw, token, 0) };
            if ok == 0 {
                return Err(DbgFlowError::Backend(format!(
                    "CreateEnvironmentBlock failed: {}",
                    last_error()
                )));
            }
            let _guard = EnvironmentGuard(raw);
            let mut env = parse_environment_block(raw as *const u16);
            apply_env_changes(&mut env, env_changes);
            Ok(Self {
                data: build_environment_block(&env),
            })
        }

        fn current(env_changes: &[EnvChange]) -> Result<Self> {
            let mut env = BTreeMap::new();
            for (key, value) in std::env::vars_os() {
                let key_text = key.to_string_lossy();
                if key_text.is_empty() {
                    continue;
                }
                let mut entry = key.clone();
                entry.push("=");
                entry.push(value);
                env.insert(key_text.to_ascii_uppercase(), entry);
            }
            apply_env_changes(&mut env, env_changes);
            Ok(Self {
                data: build_environment_block(&env),
            })
        }

        fn as_ptr(&self) -> *const std::ffi::c_void {
            self.data.as_ptr().cast()
        }
    }

    struct EnvironmentGuard(*mut std::ffi::c_void);

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyEnvironmentBlock(self.0);
            }
        }
    }

    struct AttributeList {
        ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
        _buffer: Vec<u8>,
    }

    impl AttributeList {
        fn new(handles: &mut [HANDLE]) -> Result<Self> {
            let mut size = 0usize;
            unsafe {
                let _ = InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            }
            if size == 0 {
                return Err(DbgFlowError::Backend(format!(
                    "InitializeProcThreadAttributeList size query failed: {}",
                    last_error()
                )));
            }
            let mut buffer = vec![0u8; size];
            let ptr = buffer.as_mut_ptr().cast();
            let ok = unsafe { InitializeProcThreadAttributeList(ptr, 1, 0, &mut size) };
            if ok == 0 {
                return Err(DbgFlowError::Backend(format!(
                    "InitializeProcThreadAttributeList failed: {}",
                    last_error()
                )));
            }
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    ptr,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_mut_ptr().cast(),
                    std::mem::size_of_val(handles),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if ok == 0 {
                unsafe {
                    DeleteProcThreadAttributeList(ptr);
                }
                return Err(DbgFlowError::Backend(format!(
                    "UpdateProcThreadAttribute HANDLE_LIST failed: {}",
                    last_error()
                )));
            }
            Ok(Self {
                ptr,
                _buffer: buffer,
            })
        }

        fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.ptr
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.ptr);
            }
        }
    }

    fn parse_environment_block(mut ptr: *const u16) -> BTreeMap<String, OsString> {
        let mut env = BTreeMap::new();
        unsafe {
            while !ptr.is_null() && *ptr != 0 {
                let mut len = 0;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let entry = OsString::from_wide(std::slice::from_raw_parts(ptr, len));
                let text = entry.to_string_lossy();
                if let Some((key, _)) = text.split_once('=') {
                    env.insert(key.to_ascii_uppercase(), entry);
                }
                ptr = ptr.add(len + 1);
            }
        }
        env
    }

    fn apply_env_changes(env: &mut BTreeMap<String, OsString>, changes: &[EnvChange]) {
        for change in changes {
            let key = change.key.to_string_lossy().to_string();
            let lookup = key.to_ascii_uppercase();
            match &change.value {
                Some(value) => {
                    let mut entry = OsString::from(&change.key);
                    entry.push("=");
                    entry.push(value);
                    env.insert(lookup, entry);
                }
                None => {
                    env.remove(&lookup);
                }
            }
        }
    }

    fn build_environment_block(env: &BTreeMap<String, OsString>) -> Vec<u16> {
        let mut data = Vec::new();
        for entry in env.values() {
            data.extend(entry.encode_wide());
            data.push(0);
        }
        data.push(0);
        data
    }

    #[derive(Debug)]
    struct HandleGuard(HANDLE);

    impl HandleGuard {
        fn take(&mut self) -> HANDLE {
            let handle = self.0;
            self.0 = 0;
            handle
        }
    }

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    pub(super) fn windows_try_wait(process: HANDLE) -> Result<Option<ProcessExit>> {
        let mut code = 0;
        let ok = unsafe { GetExitCodeProcess(process, &mut code) };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "GetExitCodeProcess failed: {}",
                last_error()
            )));
        }
        if code == STILL_ACTIVE_EXIT {
            Ok(None)
        } else {
            Ok(Some(ProcessExit {
                code: Some(code as i32),
            }))
        }
    }

    pub(super) fn windows_wait(process: HANDLE) -> Result<ProcessExit> {
        let wait = unsafe { WaitForSingleObject(process, INFINITE) };
        if wait == WAIT_FAILED {
            return Err(DbgFlowError::Backend(format!(
                "WaitForSingleObject failed: {}",
                last_error()
            )));
        }
        if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
            return Err(DbgFlowError::Backend(format!(
                "unexpected WaitForSingleObject result {wait}"
            )));
        }
        windows_try_wait(process).map(|exit| exit.unwrap_or(ProcessExit { code: None }))
    }

    pub(super) fn windows_terminate(process: HANDLE) -> Result<()> {
        let ok = unsafe { TerminateProcess(process, 1) };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "TerminateProcess failed: {}",
                last_error()
            )));
        }
        Ok(())
    }

    pub(super) fn windows_process_session_id(pid: u32) -> Result<u32> {
        let mut session_id = 0;
        let ok = unsafe { ProcessIdToSessionId(pid, &mut session_id) };
        if ok == 0 {
            return Err(DbgFlowError::Backend(format!(
                "ProcessIdToSessionId({pid}) failed: {}",
                last_error()
            )));
        }
        Ok(session_id)
    }

    pub(super) fn windows_tcp_owner_pid(local: SocketAddr, peer: SocketAddr) -> Result<u32> {
        if !local.ip().is_loopback() || !peer.ip().is_loopback() {
            return Err(DbgFlowError::Backend(
                "TCP peer owner lookup requires loopback endpoints".to_string(),
            ));
        }
        match (local.ip(), peer.ip()) {
            (IpAddr::V4(local_ip), IpAddr::V4(peer_ip)) => {
                tcp4_owner_pid(local_ip, local.port(), peer_ip, peer.port())
            }
            (IpAddr::V6(local_ip), IpAddr::V6(peer_ip)) => {
                tcp6_owner_pid(local_ip, local.port(), peer_ip, peer.port())
            }
            _ => Err(DbgFlowError::Backend(
                "TCP peer owner lookup requires matching IP families".to_string(),
            )),
        }
    }

    fn tcp4_owner_pid(
        server_addr: Ipv4Addr,
        server_port: u16,
        peer_addr: Ipv4Addr,
        peer_port: u16,
    ) -> Result<u32> {
        let buffer = tcp_table(AF_INET as u32)?;
        let table = buffer.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
        let count = unsafe { (*table).dwNumEntries as usize };
        let rows = unsafe { std::slice::from_raw_parts((*table).table.as_ptr(), count) };
        for row in rows {
            let row_local = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
            let row_remote = Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes());
            if row_local == peer_addr
                && tcp_port(row.dwLocalPort) == peer_port
                && row_remote == server_addr
                && tcp_port(row.dwRemotePort) == server_port
            {
                return Ok(row.dwOwningPid);
            }
        }
        Err(DbgFlowError::Backend(
            "TCP peer process was not found".to_string(),
        ))
    }

    fn tcp6_owner_pid(
        server_addr: Ipv6Addr,
        server_port: u16,
        peer_addr: Ipv6Addr,
        peer_port: u16,
    ) -> Result<u32> {
        let buffer = tcp_table(AF_INET6 as u32)?;
        let table = buffer.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID;
        let count = unsafe { (*table).dwNumEntries as usize };
        let rows = unsafe { std::slice::from_raw_parts((*table).table.as_ptr(), count) };
        for row in rows {
            let row_local = Ipv6Addr::from(row.ucLocalAddr);
            let row_remote = Ipv6Addr::from(row.ucRemoteAddr);
            if row_local == peer_addr
                && tcp_port(row.dwLocalPort) == peer_port
                && row_remote == server_addr
                && tcp_port(row.dwRemotePort) == server_port
            {
                return Ok(row.dwOwningPid);
            }
        }
        Err(DbgFlowError::Backend(
            "TCP6 peer process was not found".to_string(),
        ))
    }

    fn tcp_table(address_family: u32) -> Result<Vec<u8>> {
        let mut size = 0;
        let first = unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                address_family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
            return Err(DbgFlowError::Backend(format!(
                "GetExtendedTcpTable size query failed: {first}"
            )));
        }
        let mut buffer = vec![0u8; size as usize];
        let result = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &mut size,
                0,
                address_family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if result != NO_ERROR {
            return Err(DbgFlowError::Backend(format!(
                "GetExtendedTcpTable failed: {result}"
            )));
        }
        Ok(buffer)
    }

    fn tcp_port(value: u32) -> u16 {
        u16::from_be((value & 0xffff) as u16)
    }

    fn to_wide_null(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn to_wide_null_str(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn command_line(executable: &Path, args: &[OsString]) -> String {
        let mut parts = Vec::with_capacity(args.len() + 1);
        parts.push(quote_windows_arg(&executable.display().to_string()));
        parts.extend(
            args.iter()
                .map(|arg| quote_windows_arg(&arg.to_string_lossy())),
        );
        parts.join(" ")
    }

    fn quote_windows_arg(arg: &str) -> String {
        if arg.is_empty() {
            return "\"\"".to_string();
        }
        let needs_quotes = arg
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\t'));
        if !needs_quotes {
            return arg.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    quoted.push(ch);
                }
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn last_error() -> std::io::Error {
        let _ = unsafe { GetLastError() };
        std::io::Error::last_os_error()
    }
}

#[cfg(windows)]
use windows_impl::{
    windows_process_session_id, windows_spawn_process, windows_tcp_owner_pid, windows_terminate,
    windows_try_wait, windows_wait,
};

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn hide_console_window_sets_create_no_window_flag() {
        let mut spec = ProcessLaunchSpec::new(system32_exe("cmd.exe"));

        spec.hide_console_window();

        assert_ne!(
            spec.creation_flags & windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
            0
        );
    }

    #[test]
    fn current_process_launch_applies_environment_changes() {
        let mut spec = ProcessLaunchSpec::new(system32_exe("cmd.exe"));
        spec.args = vec!["/C".into(), "echo %DBGFLOW_PROCESS_TEST_ENV%".into()];
        spec.env = vec![EnvChange::set(
            "DBGFLOW_PROCESS_TEST_ENV",
            "env-from-launcher",
        )];
        spec.stdout = LaunchStdio::Piped;

        let mut child = spawn_process(&spec, &ProcessLaunchContext::default()).expect("spawn cmd");
        let mut stdout = String::new();
        child
            .take_stdout()
            .expect("stdout")
            .read_to_string(&mut stdout)
            .expect("read stdout");
        let exit = child.wait().expect("wait child");

        assert_eq!(exit.code, Some(0));
        assert_eq!(stdout.trim(), "env-from-launcher");
        assert_eq!(
            child.audit().resolved_source,
            ProcessIdentitySource::CurrentProcess
        );
    }

    #[test]
    fn invalid_peer_session_token_falls_back_to_current_process_with_reason() {
        let mut spec = ProcessLaunchSpec::new(system32_exe("cmd.exe"));
        spec.args = vec!["/C".into(), "exit 0".into()];
        let context = ProcessLaunchContext::new(
            ProcessLaunchConfig {
                child_identity: ChildIdentity::McpPeerSession,
                fallback_child_identity: FallbackChildIdentity::CurrentProcess,
                elevate_if_admin: true,
            },
            ToolCallContext {
                peer_pid: Some(1234),
                peer_session_id: Some(u32::MAX - 2),
            },
        );

        let mut child = spawn_process(&spec, &context).expect("spawn fallback child");
        let exit = child.wait().expect("wait child");

        assert_eq!(exit.code, Some(0));
        assert_eq!(
            child.audit().resolved_source,
            ProcessIdentitySource::CurrentProcess
        );
        let reason = child
            .audit()
            .fallback_reason
            .as_deref()
            .expect("fallback reason");
        assert!(reason.contains("MCP peer session token could not be opened"));
        assert!(reason.contains("WTSQueryUserToken"));
    }

    fn system32_exe(name: &str) -> PathBuf {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join(name)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn hide_console_window_is_noop_on_non_windows() {
        let mut spec = ProcessLaunchSpec::new("dbgflow-test");

        spec.hide_console_window();

        assert_eq!(spec.creation_flags, 0);
    }
}
