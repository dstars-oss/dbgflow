use super::ProfileCollectorConfig;
use crate::Result;
use std::path::Path;

pub trait ProfileCollector: Send + Sync {
    fn start(&self, output_dir: &Path) -> Result<CollectorStart>;
    fn stop(&self) -> Result<CollectorStop>;
    fn cleanup(&self) -> Result<()>;
}

pub trait CollectorFactory: Send + Sync {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStart {
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStop {
    pub warnings: Vec<String>,
}
