use dbgflow_core::artifacts::ArtifactKind;
use dbgflow_core::profile::{
    validate_profile_target, CollectorFactory, CollectorStart, CollectorStop, ProfileCollector,
    ProfileCollectorConfig, ProfileCollectorKind, ProfileCompletionReason, ProfileManager,
    ProfilePreset, ProfileStatus, ProfileTarget, RunProfile, TargetExit, TargetRunner,
};
use dbgflow_core::Result;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn profile_launch_target_rejects_missing_executable() {
    let missing = std::env::temp_dir()
        .join(format!("dbgflow-profile-missing-{}", std::process::id()))
        .join("missing.exe");

    let error = validate_profile_target(ProfileTarget::Launch {
        executable: missing,
        args: Vec::new(),
    })
    .expect_err("reject missing executable");

    assert!(error
        .to_string()
        .contains("invalid profile launch executable"));
}

#[test]
fn profile_launch_target_canonicalizes_existing_executable_and_rejects_nul_args() {
    let root = std::env::temp_dir().join(format!("dbgflow-profile-target-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create root");
    let executable = root.join("target.exe");
    fs::write(&executable, b"not a real exe").expect("write target");

    let target = validate_profile_target(ProfileTarget::Launch {
        executable: executable.clone(),
        args: vec!["--case".to_string(), "1".to_string()],
    })
    .expect("validate target");

    let ProfileTarget::Launch {
        executable: validated,
        args,
    } = target;
    assert_eq!(validated, executable.canonicalize().expect("canonicalize"));
    assert_eq!(args, vec!["--case".to_string(), "1".to_string()]);

    let error = validate_profile_target(ProfileTarget::Launch {
        executable,
        args: vec!["bad\0arg".to_string()],
    })
    .expect_err("reject nul argument");
    assert!(error.to_string().contains("NUL"));
}

#[test]
fn default_profile_collector_is_native_etw_system_overview() {
    let config = ProfileCollectorConfig::default();

    assert_eq!(config.kind, ProfileCollectorKind::NativeEtw);
    assert_eq!(config.preset, ProfilePreset::SystemOverview);
}

#[test]
fn fake_collector_records_start_and_stop_calls() {
    let state = Arc::new(Mutex::new(Vec::new()));
    let collector = TestCollector {
        state: state.clone(),
        fail_start: false,
        fail_stop: false,
    };
    let output_dir = std::env::temp_dir();

    let started = collector.start(&output_dir).expect("start collector");
    assert_eq!(started.warnings, Vec::<String>::new());
    let stopped = collector.stop().expect("stop collector");
    assert_eq!(stopped.warnings, Vec::<String>::new());

    assert_eq!(
        state.lock().expect("state").as_slice(),
        &["start".to_string(), "stop".to_string()]
    );
}

#[test]
fn target_runner_returns_exit_or_timeout_without_killing_target() {
    let runner = TestTargetRunner {
        exit: TargetExit::TimedOut { pid: 42 },
    };
    let exit = runner
        .launch_and_wait(
            &ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            Duration::from_millis(1),
            Path::new("stdout.txt"),
            Path::new("stderr.txt"),
        )
        .expect("launch target");

    assert_eq!(exit, TargetExit::TimedOut { pid: 42 });
}

struct TestCollector {
    state: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
}

impl ProfileCollector for TestCollector {
    fn start(&self, _output_dir: &Path) -> Result<CollectorStart> {
        self.state.lock().expect("state").push("start".to_string());
        if self.fail_start {
            return Err(dbgflow_core::DbgFlowError::Backend(
                "collector start failed".to_string(),
            ));
        }
        Ok(CollectorStart {
            warnings: Vec::new(),
        })
    }

    fn stop(&self) -> Result<CollectorStop> {
        self.state.lock().expect("state").push("stop".to_string());
        if self.fail_stop {
            return Err(dbgflow_core::DbgFlowError::Backend(
                "collector stop failed".to_string(),
            ));
        }
        Ok(CollectorStop {
            warnings: Vec::new(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        self.state
            .lock()
            .expect("state")
            .push("cleanup".to_string());
        Ok(())
    }
}

struct TestTargetRunner {
    exit: TargetExit,
}

impl TargetRunner for TestTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        Ok(self.exit.clone())
    }
}

#[test]
fn run_profile_starts_collector_launches_target_stops_collector_and_writes_artifacts() {
    let root = test_profile_root("completed");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
        }),
        Arc::new(TestTargetRunner {
            exit: TargetExit::Exited {
                pid: 1234,
                exit_code: Some(7),
            },
        }),
    );

    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collector: ProfileCollectorConfig::default(),
    };

    let result = manager.run_profile(request).expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(
        result.completion_reason,
        ProfileCompletionReason::TargetExited
    );
    assert_eq!(result.target_pid, Some(1234));
    assert_eq!(result.target_exit_code, Some(7));
    assert_eq!(result.artifacts.trace.kind, ArtifactKind::ProfileTrace);
    assert!(result.artifacts.profile.path.is_file());
    assert!(result.artifacts.events.path.is_file());
    assert!(result.artifacts.stdout.path.is_file());
    assert!(result.artifacts.stderr.path.is_file());
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &["start".to_string(), "stop".to_string()]
    );

    let metadata =
        fs::read_to_string(&result.artifacts.profile.path).expect("read profile metadata");
    assert!(metadata.contains("target_exited"));
    let events = fs::read_to_string(&result.artifacts.events.path).expect("read events");
    assert!(events.contains("collector_started"));
    assert!(events.contains("target_started"));
    assert!(events.contains("collector_stopped"));
    assert!(events.contains("profile_completed"));
}

#[test]
fn run_profile_timeout_stops_collector_without_target_exit_code() {
    let root = test_profile_root("timed-out");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(TestTargetRunner {
            exit: TargetExit::TimedOut { pid: 88 },
        }),
    );

    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1,
            collector: ProfileCollectorConfig::default(),
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::TimedOut);
    assert_eq!(result.completion_reason, ProfileCompletionReason::Timeout);
    assert_eq!(result.target_pid, Some(88));
    assert_eq!(result.target_exit_code, None);
}

#[test]
fn run_profile_collector_start_failure_does_not_launch_target() {
    let root = test_profile_root("collector-start-failure");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: Arc::new(Mutex::new(Vec::new())),
            fail_start: true,
            fail_stop: false,
        }),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1,
            collector: ProfileCollectorConfig::default(),
        })
        .expect_err("collector start fails");

    assert!(error.to_string().contains("collector start failed"));
}

fn test_profile_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("dbgflow-profile-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create root");
    root
}

#[derive(Default)]
struct TestCollectorFactory {
    state: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
}

impl CollectorFactory for TestCollectorFactory {
    fn create(
        &self,
        _config: &ProfileCollectorConfig,
        _trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        Ok(Box::new(TestCollector {
            state: self.state.clone(),
            fail_start: self.fail_start,
            fail_stop: self.fail_stop,
        }))
    }
}

struct PanicTargetRunner;

impl TargetRunner for PanicTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        panic!("target runner must not be called when collector start fails");
    }
}
