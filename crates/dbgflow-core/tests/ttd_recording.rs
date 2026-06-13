use dbgflow_common::process::ProcessLaunchContext;
use dbgflow_core::artifacts::ArtifactKind;
use dbgflow_core::logging::LogSink;
use dbgflow_core::ttd::{
    build_ttd_args, validate_ttd_options, validate_ttd_target, RecordTtd, TtdRecordMode,
    TtdRecorderExit, TtdRecorderInvocation, TtdRecorderRunner, TtdRecorderRuntime,
    TtdRecordingCompletionReason, TtdRecordingManager, TtdRecordingOptions, TtdRecordingStatus,
    TtdReplayCpuSupport, TtdStopInvocation, TtdTarget,
};
use dbgflow_core::Result;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn ttd_launch_target_canonicalizes_executable_and_rejects_nul_args() {
    let root = test_root("target-launch");
    let executable = root.join("target.exe");
    fs::write(&executable, b"fake").expect("write target");

    let target = validate_ttd_target(TtdTarget::Launch {
        executable: executable.clone(),
        args: vec!["--case".to_string(), "1".to_string()],
    })
    .expect("validate launch");

    let TtdTarget::Launch {
        executable: validated,
        args,
    } = target
    else {
        panic!("expected launch target");
    };
    assert_eq!(validated, executable.canonicalize().expect("canonicalize"));
    assert_eq!(args, vec!["--case".to_string(), "1".to_string()]);

    let error = validate_ttd_target(TtdTarget::Launch {
        executable,
        args: vec!["bad\0arg".to_string()],
    })
    .expect_err("reject NUL");
    assert!(error.to_string().contains("NUL"));
}

#[test]
fn ttd_monitor_target_rejects_relative_path_traversal() {
    let error = validate_ttd_target(TtdTarget::Monitor {
        program: PathBuf::from("..\\notepad.exe"),
        cmd_line_filter: None,
    })
    .expect_err("reject traversal");

    assert!(error.to_string().contains("path traversal"));
}

#[test]
fn ttd_options_reject_invalid_max_file() {
    let mut options = TtdRecordingOptions {
        max_file_mb: 0,
        ..Default::default()
    };
    let error =
        validate_ttd_options(&options, &TtdTarget::Attach { pid: 1 }).expect_err("reject zero");
    assert!(error.to_string().contains("greater than zero"));

    options.max_file_mb = 32769;
    options.ring = true;
    let error = validate_ttd_options(&options, &TtdTarget::Attach { pid: 1 })
        .expect_err("reject large ring");
    assert!(error.to_string().contains("32768"));
}

#[test]
fn ttd_launch_args_are_built_without_shell_command_line() {
    let target = TtdTarget::Launch {
        executable: PathBuf::from(r"C:\app\app.exe"),
        args: vec!["--flag".to_string(), "value with spaces".to_string()],
    };
    let options = TtdRecordingOptions {
        children: true,
        accept_eula: true,
        ring: true,
        max_file_mb: 256,
        modules: vec!["app.exe".to_string()],
        record_mode: TtdRecordMode::Manual,
        replay_cpu_support: TtdReplayCpuSupport::IntelAvx2Required,
        ..Default::default()
    };

    let args = build_ttd_args(&target, &options, Path::new(r"C:\dbgflow\traces"));
    let args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        args,
        vec![
            "-out",
            r"C:\dbgflow\traces",
            "-noUI",
            "-accepteula",
            "-children",
            "-ring",
            "-maxFile",
            "256",
            "-module",
            "app.exe",
            "-recordmode",
            "Manual",
            "-replayCpuSupport",
            "IntelAvx2Required",
            "-launch",
            r"C:\app\app.exe",
            "--flag",
            "value with spaces",
        ]
    );
}

#[test]
fn record_ttd_with_fake_runner_writes_artifacts_and_metadata() {
    let root = test_root("completed");
    let ttd_dir = fake_ttd_dir(&root);
    let runner = Arc::new(FakeTtdRunner::new(FakeRun {
        exit: Ok(TtdRecorderExit {
            exit_code: Some(0),
            timed_out: false,
        }),
        stdout: "Recording process (PID:1234) on trace file: app01.run\n".to_string(),
        stderr: String::new(),
        files: vec![
            ("app01.run".to_string(), b"trace".to_vec()),
            ("app01.out".to_string(), b"out".to_vec()),
        ],
    }));
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(ttd_dir),
        runner.clone(),
    );

    let result = manager
        .record_ttd(RecordTtd {
            target: TtdTarget::Attach { pid: 1234 },
            timeout_ms: 1000,
            options: Default::default(),
        })
        .expect("record ttd");

    assert_eq!(result.status, TtdRecordingStatus::Completed);
    assert_eq!(
        result.completion_reason,
        TtdRecordingCompletionReason::TargetExited
    );
    assert_eq!(result.target_pid, Some(1234));
    assert_eq!(result.artifacts.traces.len(), 1);
    assert_eq!(result.artifacts.traces[0].kind, ArtifactKind::TtdTrace);
    assert_eq!(result.artifacts.recorder_logs.len(), 1);
    assert!(result.artifacts.metadata.path.is_file());
    assert!(result.artifacts.events.path.is_file());
    assert!(result.artifacts.recorder_stdout.path.is_file());
    assert!(result.artifacts.recorder_stderr.path.is_file());

    let metadata =
        fs::read_to_string(&result.artifacts.metadata.path).expect("read recording metadata");
    assert!(metadata.contains("target_exited"));
    assert!(metadata.contains("app01.run"));
    let events = fs::read_to_string(&result.artifacts.events.path).expect("read events");
    assert!(events.contains("ttd_recording_started"));
    assert!(events.contains("trace_detected"));
    assert!(events.contains("recording_completed"));

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert!(invocations[0]
        .args
        .iter()
        .any(|arg| arg == &OsString::from("-attach")));
}

#[test]
fn record_ttd_discovers_multiple_run_files() {
    let root = test_root("multi-run");
    let ttd_dir = fake_ttd_dir(&root);
    let runner = Arc::new(FakeTtdRunner::new(FakeRun {
        exit: Ok(TtdRecorderExit {
            exit_code: Some(0),
            timed_out: false,
        }),
        stdout: String::new(),
        stderr: String::new(),
        files: vec![
            ("parent01.run".to_string(), b"parent".to_vec()),
            ("child01.run".to_string(), b"child".to_vec()),
            ("child01.idx".to_string(), b"idx".to_vec()),
        ],
    }));
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(ttd_dir),
        runner,
    );

    let result = manager
        .record_ttd(RecordTtd {
            target: TtdTarget::Attach { pid: 7 },
            timeout_ms: 1000,
            options: TtdRecordingOptions {
                children: true,
                ..Default::default()
            },
        })
        .expect("record ttd");

    assert_eq!(result.status, TtdRecordingStatus::Completed);
    assert_eq!(result.artifacts.traces.len(), 2);
    assert_eq!(result.artifacts.trace_indexes.len(), 1);
}

#[test]
fn record_ttd_nonzero_recorder_exit_returns_failed_result() {
    let root = test_root("failed");
    let ttd_dir = fake_ttd_dir(&root);
    let runner = Arc::new(FakeTtdRunner::new(FakeRun {
        exit: Ok(TtdRecorderExit {
            exit_code: Some(27),
            timed_out: false,
        }),
        stdout: "Recording process Notepad.exe(15904) From parent process explorer.exe(8440)\n"
            .to_string(),
        stderr: "recording failed\n".to_string(),
        files: vec![("bad.err".to_string(), b"error".to_vec())],
    }));
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(ttd_dir),
        runner,
    );

    let result = manager
        .record_ttd(RecordTtd {
            target: TtdTarget::Attach { pid: 99 },
            timeout_ms: 1000,
            options: Default::default(),
        })
        .expect("failed result");

    assert_eq!(result.status, TtdRecordingStatus::Failed);
    assert_eq!(
        result.completion_reason,
        TtdRecordingCompletionReason::RecorderError
    );
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("recording failed")));
    assert_eq!(result.artifacts.recorder_logs.len(), 1);
}

#[test]
fn record_ttd_monitor_timeout_runs_stop_all() {
    let root = test_root("timeout-monitor");
    let ttd_dir = fake_ttd_dir(&root);
    let runner = Arc::new(FakeTtdRunner::new(FakeRun {
        exit: Ok(TtdRecorderExit {
            exit_code: None,
            timed_out: true,
        }),
        stdout: "Recording process Notepad.exe(15904) From parent process explorer.exe(8440)\n"
            .to_string(),
        stderr: String::new(),
        files: vec![("notepad01.run".to_string(), b"trace".to_vec())],
    }));
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(ttd_dir),
        runner.clone(),
    );

    let result = manager
        .record_ttd(RecordTtd {
            target: TtdTarget::Monitor {
                program: PathBuf::from("notepad.exe"),
                cmd_line_filter: Some("specialfile.txt".to_string()),
            },
            timeout_ms: 1000,
            options: Default::default(),
        })
        .expect("timed out result");

    assert_eq!(result.status, TtdRecordingStatus::TimedOut);
    assert_eq!(
        result.completion_reason,
        TtdRecordingCompletionReason::Timeout
    );
    let stops = runner.stops();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].stop_target, OsString::from("all"));
    assert_eq!(
        fs::read_to_string(&result.artifacts.recorder_stdout.path).expect("read stdout"),
        "Recording process Notepad.exe(15904) From parent process explorer.exe(8440)\n"
    );
    assert!(result
        .artifacts
        .recorder_stop_stdout
        .as_ref()
        .expect("stop stdout")
        .path
        .is_file());
}

#[test]
fn record_ttd_attach_timeout_stops_recording_by_pid() {
    let root = test_root("timeout-attach");
    let ttd_dir = fake_ttd_dir(&root);
    let runner = Arc::new(FakeTtdRunner::new(FakeRun {
        exit: Ok(TtdRecorderExit {
            exit_code: None,
            timed_out: true,
        }),
        stdout: "Recording process (PID:4321) on trace file: app01.run\n".to_string(),
        stderr: String::new(),
        files: vec![("app01.run".to_string(), b"trace".to_vec())],
    }));
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(ttd_dir),
        runner.clone(),
    );

    let result = manager
        .record_ttd(RecordTtd {
            target: TtdTarget::Attach { pid: 4321 },
            timeout_ms: 1000,
            options: Default::default(),
        })
        .expect("timed out result");

    assert_eq!(result.status, TtdRecordingStatus::TimedOut);
    assert_eq!(result.target_pid, Some(4321));
    let stops = runner.stops();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].stop_target, OsString::from("4321"));
    assert_eq!(
        fs::read_to_string(&result.artifacts.recorder_stdout.path).expect("read stdout"),
        "Recording process (PID:4321) on trace file: app01.run\n"
    );
    assert_eq!(
        fs::read_to_string(
            &result
                .artifacts
                .recorder_stop_stdout
                .as_ref()
                .expect("stop stdout")
                .path
        )
        .expect("read stop stdout"),
        "stop output\n"
    );
}

#[test]
fn record_ttd_rejects_concurrent_job() {
    let root = test_root("concurrent");
    let ttd_dir = fake_ttd_dir(&root);
    let blocker = Arc::new((Mutex::new(BlockingState::default()), Condvar::new()));
    let runner = Arc::new(BlockingTtdRunner {
        blocker: blocker.clone(),
    });
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(ttd_dir),
        runner,
    );
    let request = RecordTtd {
        target: TtdTarget::Attach { pid: 1 },
        timeout_ms: 1000,
        options: Default::default(),
    };
    let first_manager = manager.clone();
    let first_request = request.clone();
    let first = std::thread::spawn(move || first_manager.record_ttd(first_request));

    wait_until_blocking_runner_started(&blocker);
    let error = manager
        .record_ttd(request)
        .expect_err("second recording should be rejected");
    assert!(error.to_string().contains("already active"));

    release_blocking_runner(&blocker);
    first
        .join()
        .expect("first thread")
        .expect("first recording");
}

#[test]
fn record_ttd_unavailable_runtime_returns_clear_error_without_artifacts() {
    let root = test_root("unavailable");
    let manager = TtdRecordingManager::with_components(
        &root,
        TtdRecorderRuntime::with_ttd_dir(root.join("missing-ttd")),
        Arc::new(FakeTtdRunner::new(FakeRun::success())),
    );

    let error = manager
        .record_ttd(RecordTtd {
            target: TtdTarget::Attach { pid: 1 },
            timeout_ms: 1000,
            options: Default::default(),
        })
        .expect_err("TTD unavailable");

    assert!(error.to_string().contains("TTD.exe"));
    assert!(!root.join("ttd_recordings").exists());
}

#[derive(Debug)]
struct FakeRun {
    exit: Result<TtdRecorderExit>,
    stdout: String,
    stderr: String,
    files: Vec<(String, Vec<u8>)>,
}

impl FakeRun {
    fn success() -> Self {
        Self {
            exit: Ok(TtdRecorderExit {
                exit_code: Some(0),
                timed_out: false,
            }),
            stdout: String::new(),
            stderr: String::new(),
            files: vec![("trace01.run".to_string(), b"trace".to_vec())],
        }
    }
}

struct FakeTtdRunner {
    run: Mutex<FakeRun>,
    invocations: Mutex<Vec<TtdRecorderInvocation>>,
    stops: Mutex<Vec<TtdStopInvocation>>,
}

impl FakeTtdRunner {
    fn new(run: FakeRun) -> Self {
        Self {
            run: Mutex::new(run),
            invocations: Mutex::new(Vec::new()),
            stops: Mutex::new(Vec::new()),
        }
    }

    fn invocations(&self) -> Vec<TtdRecorderInvocation> {
        self.invocations.lock().expect("invocations").clone()
    }

    fn stops(&self) -> Vec<TtdStopInvocation> {
        self.stops.lock().expect("stops").clone()
    }
}

impl TtdRecorderRunner for FakeTtdRunner {
    fn run(
        &self,
        invocation: TtdRecorderInvocation,
        _launch_context: ProcessLaunchContext,
        _logger: Arc<dyn LogSink>,
    ) -> Result<TtdRecorderExit> {
        self.invocations
            .lock()
            .expect("invocations")
            .push(invocation.clone());
        let run = self.run.lock().expect("run");
        fs::write(&invocation.stdout_path, &run.stdout).expect("write fake stdout");
        fs::write(&invocation.stderr_path, &run.stderr).expect("write fake stderr");
        if let Some(traces_dir) = traces_dir_from_args(&invocation.args) {
            fs::create_dir_all(&traces_dir).expect("create traces dir");
            for (name, contents) in &run.files {
                fs::write(traces_dir.join(name), contents).expect("write fake trace file");
            }
        }
        match &run.exit {
            Ok(exit) => Ok(exit.clone()),
            Err(error) => Err(dbgflow_core::DbgFlowError::Backend(error.to_string())),
        }
    }

    fn stop(
        &self,
        invocation: TtdStopInvocation,
        _launch_context: ProcessLaunchContext,
        _logger: Arc<dyn LogSink>,
    ) -> Result<TtdRecorderExit> {
        fs::write(&invocation.stdout_path, b"stop output\n").expect("write stop stdout");
        fs::write(&invocation.stderr_path, b"").expect("write stop stderr");
        self.stops.lock().expect("stops").push(invocation);
        Ok(TtdRecorderExit {
            exit_code: Some(0),
            timed_out: false,
        })
    }
}

struct BlockingTtdRunner {
    blocker: Arc<(Mutex<BlockingState>, Condvar)>,
}

impl TtdRecorderRunner for BlockingTtdRunner {
    fn run(
        &self,
        invocation: TtdRecorderInvocation,
        _launch_context: ProcessLaunchContext,
        _logger: Arc<dyn LogSink>,
    ) -> Result<TtdRecorderExit> {
        if let Some(traces_dir) = traces_dir_from_args(&invocation.args) {
            fs::create_dir_all(&traces_dir).expect("create traces dir");
            fs::write(traces_dir.join("blocked.run"), b"trace").expect("write trace");
        }
        fs::write(&invocation.stdout_path, b"").expect("write stdout");
        fs::write(&invocation.stderr_path, b"").expect("write stderr");
        let (lock, cvar) = &*self.blocker;
        let mut state = lock.lock().expect("blocker lock");
        state.started = true;
        cvar.notify_all();
        while !state.released {
            state = cvar.wait(state).expect("blocker wait");
        }
        Ok(TtdRecorderExit {
            exit_code: Some(0),
            timed_out: false,
        })
    }

    fn stop(
        &self,
        _invocation: TtdStopInvocation,
        _launch_context: ProcessLaunchContext,
        _logger: Arc<dyn LogSink>,
    ) -> Result<TtdRecorderExit> {
        Ok(TtdRecorderExit {
            exit_code: Some(0),
            timed_out: false,
        })
    }
}

#[derive(Default)]
struct BlockingState {
    started: bool,
    released: bool,
}

fn traces_dir_from_args(args: &[OsString]) -> Option<PathBuf> {
    args.windows(2).find_map(|window| {
        if window[0] == OsString::from("-out") {
            Some(PathBuf::from(window[1].clone()))
        } else {
            None
        }
    })
}

fn wait_until_blocking_runner_started(blocker: &Arc<(Mutex<BlockingState>, Condvar)>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let (lock, cvar) = &**blocker;
    let mut state = lock.lock().expect("blocker lock");
    while !state.started {
        assert!(std::time::Instant::now() < deadline, "runner did not start");
        let (next, _) = cvar
            .wait_timeout(state, Duration::from_millis(10))
            .expect("wait");
        state = next;
    }
}

fn release_blocking_runner(blocker: &Arc<(Mutex<BlockingState>, Condvar)>) {
    let (lock, cvar) = &**blocker;
    let mut state = lock.lock().expect("blocker lock");
    state.released = true;
    cvar.notify_all();
}

fn fake_ttd_dir(root: &Path) -> PathBuf {
    let dir = root.join("ttd-bin");
    fs::create_dir_all(&dir).expect("create fake ttd dir");
    fs::write(dir.join("TTD.exe"), b"fake").expect("write fake TTD.exe");
    dir
}

fn test_root(name: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("dbgflow-ttd-{name}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create root");
    root
}
