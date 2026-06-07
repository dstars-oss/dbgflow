use dbgflow_core::profile::{
    validate_profile_target, ProfileCollectorConfig, ProfileCollectorKind, ProfilePreset,
    ProfileTarget,
};
use std::fs;

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
