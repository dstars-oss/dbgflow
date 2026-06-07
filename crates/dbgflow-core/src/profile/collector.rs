use super::{ProfileCollectorConfig, ProfileCollectorKind};
use crate::artifacts::ArtifactRef;
use crate::Result;
use std::path::Path;

pub trait ProfileCollector: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ProfileCollectorKind;
    fn start(&self) -> Result<CollectorStart>;
    fn stop(&self, target_pid: Option<u32>) -> Result<CollectorStop>;
    fn cleanup(&self) -> Result<()>;
}

pub trait CollectorFactory: Send + Sync {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>>;
}

pub struct DefaultProfileCollectorFactory {
    procmon: super::ProcmonRuntime,
}

impl DefaultProfileCollectorFactory {
    pub fn new(procmon: super::ProcmonRuntime) -> Self {
        Self { procmon }
    }
}

impl CollectorFactory for DefaultProfileCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        match config {
            ProfileCollectorConfig::NativeEtw { .. } => {
                super::native_etw::NativeEtwCollectorFactory.create(config, output_dir)
            }
            ProfileCollectorConfig::Procmon { .. } => {
                super::procmon::ProcmonCollectorFactory::new(self.procmon.clone())
                    .create(config, output_dir)
            }
        }
    }
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
