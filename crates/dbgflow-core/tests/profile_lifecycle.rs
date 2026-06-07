use dbgflow_core::artifacts::{ArtifactKind, ArtifactRef};
use dbgflow_core::profile::{
    validate_profile_target, CollectorFactory, CollectorStart, CollectorStop, ProfileCollector,
    ProfileCollectorConfig, ProfileCollectorKind, ProfileCompletionReason, ProfileManager,
    ProfilePreset, ProfileStatus, ProfileTarget, RunProfile, TargetExit, TargetRunner,
};
use dbgflow_core::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
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

    assert_eq!(config.kind(), ProfileCollectorKind::NativeEtw);
    assert!(matches!(
        config,
        ProfileCollectorConfig::NativeEtw {
            preset: ProfilePreset::SystemOverview
        }
    ));
}

#[test]
fn default_run_profile_collectors_is_native_etw_system_overview() {
    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collectors: Vec::new(),
    }
    .with_default_collectors();

    assert_eq!(request.collectors.len(), 1);
    assert!(matches!(
        request.collectors[0],
        ProfileCollectorConfig::NativeEtw {
            preset: ProfilePreset::SystemOverview
        }
    ));
}

#[test]
fn procmon_collector_config_defaults_to_no_stacks_and_empty_filters() {
    let config = ProfileCollectorConfig::Procmon {
        capture_stacks: false,
        filters: Default::default(),
    };

    assert_eq!(config.kind(), ProfileCollectorKind::Procmon);
    let ProfileCollectorConfig::Procmon {
        capture_stacks,
        filters,
    } = config
    else {
        panic!("expected procmon config");
    };
    assert!(!capture_stacks);
    assert!(filters.operations.is_empty());
    assert!(filters.paths.is_empty());
}

#[test]
fn fake_collector_records_start_and_stop_calls() {
    let state = Arc::new(Mutex::new(Vec::new()));
    let collector = TestCollector {
        name: "native_etw".to_string(),
        kind: ProfileCollectorKind::NativeEtw,
        artifact_path: None,
        state: state.clone(),
        fail_start: false,
        fail_stop: false,
    };

    let started = collector.start().expect("start collector");
    assert_eq!(started.warnings, Vec::<String>::new());
    let stopped = collector.stop(None).expect("stop collector");
    assert_eq!(stopped.warnings, Vec::<String>::new());

    assert_eq!(
        state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "stop:native_etw".to_string()
        ]
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
    name: String,
    kind: ProfileCollectorKind,
    artifact_path: Option<PathBuf>,
    state: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
}

impl ProfileCollector for TestCollector {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ProfileCollectorKind {
        self.kind
    }

    fn start(&self) -> Result<CollectorStart> {
        self.state
            .lock()
            .expect("state")
            .push(format!("start:{}", self.name));
        if self.fail_start {
            return Err(dbgflow_core::DbgFlowError::Backend(format!(
                "collector start failed: {}",
                self.name
            )));
        }
        Ok(CollectorStart {
            warnings: Vec::new(),
        })
    }

    fn stop(&self, _target_pid: Option<u32>) -> Result<CollectorStop> {
        self.state
            .lock()
            .expect("state")
            .push(format!("stop:{}", self.name));
        if self.fail_stop {
            return Err(dbgflow_core::DbgFlowError::Backend(format!(
                "collector stop failed: {}",
                self.name
            )));
        }
        Ok(CollectorStop {
            artifacts: self
                .artifact_path
                .clone()
                .map(|path| {
                    vec![ArtifactRef {
                        kind: ArtifactKind::ProfileCollectorTrace,
                        path,
                    }]
                })
                .unwrap_or_default(),
            warnings: Vec::new(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        self.state
            .lock()
            .expect("state")
            .push(format!("cleanup:{}", self.name));
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
            fail_start_for: None,
            fail_stop_for: None,
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
        collectors: vec![ProfileCollectorConfig::default()],
    };

    let result = manager.run_profile(request).expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(
        result.completion_reason,
        ProfileCompletionReason::TargetExited
    );
    assert_eq!(result.target_pid, Some(1234));
    assert_eq!(result.target_exit_code, Some(7));
    assert_eq!(
        result
            .artifacts
            .trace
            .as_ref()
            .expect("legacy trace artifact")
            .kind,
        ArtifactKind::ProfileTrace
    );
    assert!(result.artifacts.profile.path.is_file());
    assert!(result.artifacts.events.path.is_file());
    assert!(result.artifacts.stdout.path.is_file());
    assert!(result.artifacts.stderr.path.is_file());
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "stop:native_etw".to_string()
        ]
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
            collectors: vec![ProfileCollectorConfig::default()],
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
            fail_start_for: None,
            fail_stop_for: None,
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
            collectors: vec![ProfileCollectorConfig::default()],
        })
        .expect_err("collector start fails");

    assert!(error.to_string().contains("collector start failed"));
}

#[test]
fn run_profile_procmon_without_sysinternals_dir_does_not_launch_target() {
    let root = test_profile_root("procmon-unavailable");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(dbgflow_core::profile::DefaultProfileCollectorFactory::new(
            dbgflow_core::profile::ProcmonRuntime::unavailable(),
        )),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1,
            collectors: vec![ProfileCollectorConfig::Procmon {
                capture_stacks: false,
                filters: Default::default(),
            }],
        })
        .expect_err("procmon unavailable");

    assert!(error.to_string().contains("--sysinternals-dir"));
}

#[test]
fn run_profile_starts_and_stops_collectors_in_reverse_stop_order() {
    let root = test_profile_root("multi-collector");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
            fail_start_for: None,
            fail_stop_for: None,
        }),
        Arc::new(TestTargetRunner {
            exit: TargetExit::Exited {
                pid: 1234,
                exit_code: Some(0),
            },
        }),
    );

    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![
                ProfileCollectorConfig::NativeEtw {
                    preset: ProfilePreset::SystemOverview,
                },
                ProfileCollectorConfig::Procmon {
                    capture_stacks: true,
                    filters: Default::default(),
                },
            ],
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(result.collector_results.len(), 2);
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "start:procmon".to_string(),
            "stop:procmon".to_string(),
            "stop:native_etw".to_string(),
        ]
    );
}

#[test]
fn run_profile_start_failure_stops_already_started_collectors() {
    let root = test_profile_root("multi-start-failure");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
            fail_start_for: Some("procmon".to_string()),
            fail_stop_for: None,
        }),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![
                ProfileCollectorConfig::NativeEtw {
                    preset: ProfilePreset::SystemOverview,
                },
                ProfileCollectorConfig::Procmon {
                    capture_stacks: false,
                    filters: Default::default(),
                },
            ],
        })
        .expect_err("collector start fails");

    assert!(error
        .to_string()
        .contains("collector start failed: procmon"));
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "start:procmon".to_string(),
            "cleanup:procmon".to_string(),
            "stop:native_etw".to_string(),
        ]
    );
}

#[test]
fn run_profile_start_failure_cleans_up_failing_collector() {
    let root = test_profile_root("start-failure-cleanup");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
            fail_start_for: Some("native_etw".to_string()),
            fail_stop_for: None,
        }),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![ProfileCollectorConfig::NativeEtw {
                preset: ProfilePreset::SystemOverview,
            }],
        })
        .expect_err("collector start fails");

    assert!(error
        .to_string()
        .contains("collector start failed: native_etw"));
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "cleanup:native_etw".to_string(),
        ]
    );
}

#[test]
fn run_profile_rejects_duplicate_collector_kinds() {
    let root = test_profile_root("duplicate-collectors");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![
                ProfileCollectorConfig::NativeEtw {
                    preset: ProfilePreset::SystemOverview,
                },
                ProfileCollectorConfig::NativeEtw {
                    preset: ProfilePreset::SystemOverview,
                },
            ],
        })
        .expect_err("duplicate collectors rejected");

    assert!(error.to_string().contains("duplicate profile collector"));
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
    fail_start_for: Option<String>,
    fail_stop_for: Option<String>,
}

impl CollectorFactory for TestCollectorFactory {
    fn create(
        &self,
        _config: &ProfileCollectorConfig,
        _output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        let name = _config.artifact_name().to_string();
        let kind = _config.kind();
        let artifact_path = match kind {
            ProfileCollectorKind::NativeEtw => Some(_output_dir.join("trace.etl")),
            ProfileCollectorKind::Procmon => Some(_output_dir.join("capture.pml")),
        };
        let fail_start = self.fail_start || self.fail_start_for.as_deref() == Some(name.as_str());
        let fail_stop = self.fail_stop || self.fail_stop_for.as_deref() == Some(name.as_str());
        Ok(Box::new(TestCollector {
            name,
            kind,
            artifact_path,
            state: self.state.clone(),
            fail_start,
            fail_stop,
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

#[test]
fn run_profile_rejects_concurrent_profile_job() {
    let root = test_profile_root("concurrent");
    let blocker = Arc::new((Mutex::new(BlockingTargetState::default()), Condvar::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(BlockingTargetRunner {
            blocker: blocker.clone(),
        }),
    );
    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collectors: vec![ProfileCollectorConfig::default()],
    };
    let first_manager = manager.clone();
    let first_request = request.clone();
    let first = std::thread::spawn(move || first_manager.run_profile(first_request));

    wait_until_blocking_runner_started(&blocker);
    let error = manager
        .run_profile(request)
        .expect_err("second profile should be rejected");
    assert!(error.to_string().contains("already active"));

    release_blocking_runner(&blocker);
    first.join().expect("first thread").expect("first profile");
}

#[test]
fn target_launch_failure_after_collector_start_returns_failed_result_and_stops_collector() {
    let root = test_profile_root("target-launch-failure");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
            fail_start_for: None,
            fail_stop_for: None,
        }),
        Arc::new(FailingTargetRunner),
    );

    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![ProfileCollectorConfig::default()],
        })
        .expect("failed profile result");

    assert_eq!(result.status, ProfileStatus::Failed);
    assert_eq!(
        result.completion_reason,
        ProfileCompletionReason::TargetLaunchFailed
    );
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("target failed")));
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "stop:native_etw".to_string()
        ]
    );
}

#[test]
fn run_profile_allows_new_job_after_failed_profile() {
    let root = test_profile_root("failure-releases-active");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(SequenceTargetRunner {
            exits: Mutex::new(vec![
                Err(dbgflow_core::DbgFlowError::Backend(
                    "target failed".to_string(),
                )),
                Ok(TargetExit::Exited {
                    pid: 99,
                    exit_code: Some(0),
                }),
            ]),
        }),
    );
    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collectors: vec![ProfileCollectorConfig::default()],
    };

    let first = manager
        .run_profile(request.clone())
        .expect("first returns failed profile result");
    assert_eq!(first.status, ProfileStatus::Failed);

    let second = manager.run_profile(request).expect("second profile starts");
    assert_eq!(second.status, ProfileStatus::Completed);
    assert_eq!(second.target_pid, Some(99));
}

struct BlockingTargetRunner {
    blocker: Arc<(Mutex<BlockingTargetState>, Condvar)>,
}

impl TargetRunner for BlockingTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        let (lock, cvar) = &*self.blocker;
        let mut state = lock.lock().expect("blocker lock");
        state.started = true;
        cvar.notify_all();
        while !state.released {
            state = cvar.wait(state).expect("blocker wait");
        }
        Ok(TargetExit::Exited {
            pid: 1,
            exit_code: Some(0),
        })
    }
}

#[derive(Default)]
struct BlockingTargetState {
    started: bool,
    released: bool,
}

struct FailingTargetRunner;

impl TargetRunner for FailingTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        Err(dbgflow_core::DbgFlowError::Backend(
            "target failed".to_string(),
        ))
    }
}

struct SequenceTargetRunner {
    exits: Mutex<Vec<Result<TargetExit>>>,
}

impl TargetRunner for SequenceTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        let mut exits = self.exits.lock().expect("sequence lock");
        assert!(!exits.is_empty(), "sequence target runner exhausted");
        exits.remove(0)
    }
}

fn wait_until_blocking_runner_started(blocker: &Arc<(Mutex<BlockingTargetState>, Condvar)>) {
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

fn release_blocking_runner(blocker: &Arc<(Mutex<BlockingTargetState>, Condvar)>) {
    let (lock, cvar) = &**blocker;
    let mut state = lock.lock().expect("blocker lock");
    state.released = true;
    cvar.notify_all();
}
