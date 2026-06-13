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
    let io_path = root.join("native-etw-file-io.txt");
    let command = format!(
        "$p = '{}'; Set-Content -LiteralPath $p -Value 'dbgflow-etw'; Get-Content -LiteralPath $p | Out-Null; Remove-Item -LiteralPath $p -Force",
        io_path.display()
    );

    let manager = ProfileManager::new(&root);
    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::path::PathBuf::from(
                    r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                ),
                args: vec!["-NoProfile".to_string(), "-Command".to_string(), command],
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

    let native_etw = result
        .collector_results
        .iter()
        .find(|collector| collector.name == "native_etw")
        .expect("native_etw collector result");
    let lifecycle_artifact = native_etw
        .artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("process.jsonl"))
        .expect("process artifact");
    let file_io_artifact = native_etw
        .artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("file_io.jsonl"))
        .expect("file io artifact");
    let summary_artifact = native_etw
        .artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("summary.json"))
        .expect("summary artifact");
    let events_text = std::fs::read_to_string(&lifecycle_artifact.path).expect("read events");
    assert!(
        !events_text.trim().is_empty(),
        "expected filtered lifecycle ETW events"
    );
    let target_pid = result.target_pid.expect("target pid");
    let events = events_text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse event"))
        .collect::<Vec<_>>();
    assert!(
        events.iter().all(|event| event["pid"] == target_pid),
        "expected events to be filtered to target pid {target_pid}"
    );
    assert!(events.iter().any(|event| event["event"] == "process_start"
        || event["event"] == "process_end"
        || event["event"] == "thread_start"
        || event["event"] == "thread_end"
        || event["event"] == "image_load"
        || event["event"] == "image_unload"));
    assert!(events.iter().all(|event| event.get("stack").is_some()));

    let file_events_text =
        std::fs::read_to_string(&file_io_artifact.path).expect("read file io events");
    assert!(
        !file_events_text.trim().is_empty(),
        "expected filtered file I/O ETW events"
    );
    let file_events = file_events_text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse file event"))
        .collect::<Vec<_>>();
    assert!(
        file_events.iter().all(|event| event["pid"] == target_pid
            && event["event"] != "op_end"
            && event["event"] != "close"),
        "expected file events to be filtered to target activity for pid {target_pid}"
    );
    assert!(file_events.iter().any(|event| event["event"] == "create"
        || event["event"] == "read"
        || event["event"] == "write"));
    assert!(
        file_events
            .iter()
            .any(|event| event.get("completion_sequence").is_some()),
        "expected at least one file event enriched with OpEnd completion fields"
    );

    let summary: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&summary_artifact.path).expect("read summary"),
    )
    .expect("parse summary");
    assert_eq!(summary["target_pid"], target_pid);
    assert_eq!(summary["stacks_enabled"], true);
    assert!(summary["requested_event_sets"]
        .as_array()
        .expect("requested event sets")
        .contains(&serde_json::Value::String("process".to_string())));
    assert!(summary["requested_event_sets"]
        .as_array()
        .expect("requested event sets")
        .contains(&serde_json::Value::String("file_io".to_string())));
    assert!(summary["event_sets"].get("process").is_some());
    assert!(summary["event_sets"].get("file_io").is_some());
}
