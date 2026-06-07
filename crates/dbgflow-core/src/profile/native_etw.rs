use super::{
    CollectorFactory, ProfileCollector, ProfileCollectorConfig, ProfileCollectorKind,
    ProfilePreset,
};
use crate::{DbgFlowError, Result};
use std::path::Path;

#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        _trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        if config.kind != ProfileCollectorKind::NativeEtw
            || config.preset != ProfilePreset::SystemOverview
        {
            return Err(DbgFlowError::Backend(
                "unsupported native ETW profile collector configuration".to_string(),
            ));
        }
        Err(DbgFlowError::Backend(
            "native ETW collector is not implemented yet".to_string(),
        ))
    }
}
