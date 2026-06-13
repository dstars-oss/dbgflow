use dbgflow_core::profile::id::ProfileId;
use dbgflow_core::ttd::TtdRecordingId;

#[test]
fn compatibility_facade_keeps_legacy_id_paths() {
    let _: Option<ProfileId> = None;
    let _: Option<TtdRecordingId> = None;
}
