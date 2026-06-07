#![cfg(windows)]

use dbgflow_core::profile::{
    ProcmonRuntime, ProfileCollectorConfig, ProfileCollectorKind, ProfileManager, ProfileStatus,
    ProfileTarget, RunProfile,
};

#[test]
#[ignore = "requires Sysinternals Procmon and elevated/service-capable local environment"]
fn procmon_run_profile_writes_pml_for_cmd() {
    let sysinternals_dir = std::env::var_os("DBGFLOW_SYSINTERNALS_DIR")
        .map(std::path::PathBuf::from)
        .expect("set DBGFLOW_SYSINTERNALS_DIR to a Sysinternals directory");
    let root =
        std::env::temp_dir().join(format!("dbgflow-procmon-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");

    let manager = ProfileManager::with_runtime(
        &root,
        ProcmonRuntime::with_sysinternals_dir(sysinternals_dir),
    );
    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::path::PathBuf::from("C:\\Windows\\System32\\cmd.exe"),
                args: vec!["/C".to_string(), "echo dbgflow-procmon".to_string()],
            },
            timeout_ms: 10_000,
            collectors: vec![ProfileCollectorConfig::Procmon {
                capture_stacks: false,
                filters: Default::default(),
            }],
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    let procmon = result
        .collector_results
        .iter()
        .find(|collector| collector.kind == ProfileCollectorKind::Procmon)
        .expect("procmon result");
    assert!(procmon
        .artifacts
        .iter()
        .any(|artifact| artifact.path.extension().is_some_and(|ext| ext == "pml")));
}
