use super::registry::RunProfileRequest;
use dbgflow_common::process::ToolCallContext;
use dbgflow_common::Result;
use dbgflow_trace::profile::{ProfileManager, ProfileResult, RunProfile};
use dbgflow_trace::ttd::{RecordTtd, TtdRecordingManager, TtdRecordingResult};

pub(super) fn run_profile(
    profiles: &ProfileManager,
    request: RunProfileRequest,
    context: ToolCallContext,
) -> Result<ProfileResult> {
    profiles.run_profile_with_context(RunProfile::from(request), context)
}

pub(super) fn record_ttd(
    ttd_recordings: &TtdRecordingManager,
    request: RecordTtd,
    context: ToolCallContext,
) -> Result<TtdRecordingResult> {
    ttd_recordings.record_ttd_with_context(request, context)
}
