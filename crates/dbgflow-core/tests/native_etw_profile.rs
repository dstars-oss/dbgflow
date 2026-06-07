#![cfg(windows)]

use dbgflow_core::profile::{
    ProfileCollectorConfig, ProfileCompletionReason, ProfileManager, ProfileStatus, ProfileTarget,
    RunProfile,
};

#[test]
#[ignore = "requires local ETW permissions and writes a real ETL trace"]
fn native_etw_run_profile_writes_etl_for_cmd() {
    let root =
        std::env::temp_dir().join(format!("dbgflow-native-etw-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");

    let manager = ProfileManager::new(&root);
    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                args: vec!["/C".to_string(), "echo dbgflow-etw".to_string()],
            },
            timeout_ms: 10_000,
            collectors: vec![ProfileCollectorConfig::default()],
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(
        result.completion_reason,
        ProfileCompletionReason::TargetExited
    );
    assert_eq!(result.error, None);
    let trace = result
        .artifacts
        .trace
        .as_ref()
        .expect("legacy trace artifact");
    assert!(trace.path.is_file());
    assert!(
        trace.path.metadata().expect("trace metadata").len() > 0,
        "expected non-empty ETL"
    );

    let metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&result.artifacts.profile.path).expect("read profile metadata"),
    )
    .expect("parse profile metadata");
    assert_eq!(metadata["error"], serde_json::Value::Null);
}
