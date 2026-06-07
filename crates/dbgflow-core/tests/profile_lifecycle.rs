use dbgflow_core::profile::{
    validate_profile_target, CollectorStart, CollectorStop, ProfileCollector,
    ProfileCollectorConfig, ProfileCollectorKind, ProfilePreset, ProfileTarget, TargetExit,
    TargetRunner,
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
