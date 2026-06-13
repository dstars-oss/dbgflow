use super::registry::RunProfileRequest;
use dbgflow_common::Result;
use dbgflow_trace::profile::{ProfileManager, ProfileResult, RunProfile};
use dbgflow_trace::ttd::{RecordTtd, TtdRecordingManager, TtdRecordingResult};

pub(super) fn run_profile(
    profiles: &ProfileManager,
    request: RunProfileRequest,
) -> Result<ProfileResult> {
    profiles.run_profile(RunProfile::from(request))
}

pub(super) fn record_ttd(
    ttd_recordings: &TtdRecordingManager,
    request: RecordTtd,
) -> Result<TtdRecordingResult> {
    ttd_recordings.record_ttd(request)
}
