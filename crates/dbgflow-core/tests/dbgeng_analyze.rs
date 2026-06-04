#![cfg(windows)]

use dbgflow_core::backend::dbgeng::DbgEngBackend;
use dbgflow_core::backend::mock::MockBackend;
use dbgflow_core::backend::DebugTarget;
use dbgflow_core::session::{CreateSession, ExecuteSession, SessionManager, SessionState};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWithDataSegs, MiniDumpWithHandleData, MiniDumpWithThreadInfo, MiniDumpWriteDump,
    SetUnhandledExceptionFilter, EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS,
    MINIDUMP_EXCEPTION_INFORMATION,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
};

static CHILD_DUMP_PATH: OnceLock<PathBuf> = OnceLock::new();

#[test]
fn dbgeng_can_run_analyze_v_on_generated_dump() {
    if std::env::var_os("DBGFLOW_CRASH_DUMP_CHILD").is_some() {
        run_crashing_child();
        return;
    }

    let artifact_root = test_artifact_root("dbgeng-analyze");
    let dump_path = test_artifact_root("dbgeng-analyze-input")
        .join("test-fixtures")
        .join("crash.dmp");
    std::fs::create_dir_all(dump_path.parent().expect("dump parent")).expect("create dump dir");
    generate_crash_dump(&dump_path);
    assert!(
        dump_path.is_file(),
        "expected generated dump at {dump_path:?}"
    );

    let manager = SessionManager::with_artifact_root(
        vec![
            std::sync::Arc::new(MockBackend::new()),
            std::sync::Arc::new(DbgEngBackend::new()),
        ],
        &artifact_root,
    );
    let session = manager
        .create_session(CreateSession {
            target: DebugTarget::Dump {
                path: dump_path.clone(),
            },
            startup_timeout_ms: None,
        })
        .expect("open dump session");
    let session = wait_for_break(&manager, session.id);
    assert_eq!(session.state, SessionState::Break);

    let result = manager
        .execute(ExecuteSession {
            session_id: session.id,
            command: "!analyze -v".to_string(),
            timeout_ms: Some(120_000),
        })
        .expect("execute !analyze -v");

    let output = std::fs::read_to_string(&result.artifact.path).expect("read analyze output");
    let output_upper = output.to_ascii_uppercase();
    assert!(
        ["EXCEPTION", "FAULTING", "STACK_TEXT", "BUGCHECK"]
            .iter()
            .any(|needle| output_upper.contains(needle)),
        "unexpected !analyze -v output:\n{}",
        result.output
    );

    manager.close_session(session.id).expect("close session");
}

fn generate_crash_dump(path: &Path) {
    let current_exe = std::env::current_exe().expect("current test exe");
    let status = Command::new(current_exe)
        .env("DBGFLOW_CRASH_DUMP_CHILD", "1")
        .env("DBGFLOW_CRASH_DUMP_PATH", path)
        .status()
        .expect("run crash child");
    assert!(
        !status.success(),
        "crash dump child should terminate through an exception"
    );
}

fn run_crashing_child() {
    let dump_path =
        PathBuf::from(std::env::var_os("DBGFLOW_CRASH_DUMP_PATH").expect("dump path env"));
    CHILD_DUMP_PATH
        .set(dump_path)
        .expect("set child dump path once");

    unsafe {
        SetUnhandledExceptionFilter(Some(write_dump_on_exception));
        std::ptr::write_volatile(0x1000usize as *mut u32, 0xdead_beef);
    }
}

unsafe extern "system" fn write_dump_on_exception(
    exception_info: *const EXCEPTION_POINTERS,
) -> i32 {
    let Some(path) = CHILD_DUMP_PATH.get() else {
        return EXCEPTION_EXECUTE_HANDLER;
    };

    let wide_path = to_wide_null(path);
    let file = unsafe {
        CreateFileW(
            PCWSTR(wide_path.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };

    if let Ok(file) = file {
        let mut exception = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: unsafe { GetCurrentThreadId() },
            ExceptionPointers: exception_info as *mut EXCEPTION_POINTERS,
            ClientPointers: Default::default(),
        };
        let dump_type = MiniDumpWithDataSegs | MiniDumpWithHandleData | MiniDumpWithThreadInfo;
        let _ = unsafe {
            MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                file,
                dump_type,
                Some(&mut exception),
                None,
                None,
            )
        };
        let _ = unsafe { CloseHandle(file) };
    }

    EXCEPTION_EXECUTE_HANDLER
}

fn to_wide_null(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn test_artifact_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create artifact root");
    root
}

fn wait_for_break(
    manager: &SessionManager,
    session_id: dbgflow_core::session::SessionId,
) -> dbgflow_core::session::Session {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(130);
    loop {
        let session = manager.query_session(session_id).expect("query session");
        if session.state == SessionState::Break {
            return session;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session did not break: {session:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
