use crate::profile::{EtwEventSet, EtwProfileScope, ProfileCollectorConfig};
use dbgflow_common::{DbgFlowError, Result};

#[derive(Debug, Clone, Copy)]
pub(super) struct NativeEtwSettings {
    pub(super) event_sets: [Option<EtwEventSet>; 2],
    pub(super) stacks_enabled: bool,
}

pub(super) fn native_etw_settings(config: &ProfileCollectorConfig) -> Result<NativeEtwSettings> {
    let ProfileCollectorConfig::NativeEtw {
        scope,
        event_sets,
        stacks,
    } = config;
    if scope != &EtwProfileScope::TargetProcess {
        return Err(DbgFlowError::Backend(
            "native ETW scope must be target_process".to_string(),
        ));
    }
    if event_sets.is_empty() {
        return Err(DbgFlowError::Backend(
            "native ETW event_sets must contain at least one event set".to_string(),
        ));
    }
    let mut normalized = [None, None];
    for event_set in event_sets {
        if event_sets
            .iter()
            .filter(|candidate| *candidate == event_set)
            .count()
            > 1
        {
            return Err(DbgFlowError::Backend(format!(
                "duplicate native ETW event set is not supported: {}",
                event_set_name(*event_set)
            )));
        }
        match event_set {
            EtwEventSet::Process => normalized[0] = Some(EtwEventSet::Process),
            EtwEventSet::FileIo => normalized[1] = Some(EtwEventSet::FileIo),
        }
    }
    Ok(NativeEtwSettings {
        event_sets: normalized,
        stacks_enabled: stacks.enabled,
    })
}

impl NativeEtwSettings {
    pub(super) fn includes(&self, event_set: EtwEventSet) -> bool {
        self.event_sets.contains(&Some(event_set))
    }

    pub(super) fn event_set_names(&self) -> Vec<&'static str> {
        self.event_sets
            .iter()
            .flatten()
            .map(|event_set| event_set_name(*event_set))
            .collect()
    }
}

pub(super) fn event_set_name(event_set: EtwEventSet) -> &'static str {
    match event_set {
        EtwEventSet::Process => "process",
        EtwEventSet::FileIo => "file_io",
    }
}
