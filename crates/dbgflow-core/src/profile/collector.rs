use super::{ProfileCollectorConfig, ProfileCollectorKind};
use crate::artifacts::ArtifactRef;
use crate::Result;
use std::path::Path;

pub trait ProfileCollector: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ProfileCollectorKind;
    fn start(&self) -> Result<CollectorStart>;
    fn stop(&self) -> Result<CollectorStop>;
    fn cleanup(&self) -> Result<()>;
}

pub trait CollectorFactory: Send + Sync {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStart {
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStop {
    pub artifacts: Vec<ArtifactRef>,
    pub warnings: Vec<String>,
}
