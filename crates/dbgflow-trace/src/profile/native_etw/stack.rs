#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StackFrame {
    pub(super) value: String,
    pub(super) resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModuleInterval {
    pub(super) base: u64,
    pub(super) size: u64,
    pub(super) name: String,
    pub(super) loaded_at: i64,
    pub(super) unloaded_at: Option<i64>,
}

impl ModuleInterval {
    fn contains_at(&self, address: u64, timestamp: i64) -> bool {
        address >= self.base
            && address < self.base.saturating_add(self.size)
            && timestamp >= self.loaded_at
            && self
                .unloaded_at
                .map(|unloaded_at| timestamp <= unloaded_at)
                .unwrap_or(true)
    }
}

pub(super) fn resolve_stack_addresses(
    addresses: &[u64],
    modules: &[ModuleInterval],
    timestamp: i64,
) -> Vec<StackFrame> {
    addresses
        .iter()
        .map(|address| resolve_stack_address(*address, modules, timestamp))
        .collect()
}

pub(super) fn resolve_stack_address(
    address: u64,
    modules: &[ModuleInterval],
    timestamp: i64,
) -> StackFrame {
    if let Some(module) = modules
        .iter()
        .filter(|module| module.contains_at(address, timestamp))
        .max_by_key(|module| module.loaded_at)
    {
        let offset = address - module.base;
        return StackFrame {
            value: format!("{}+0x{offset:x}", module.name),
            resolved: true,
        };
    }
    StackFrame {
        value: hex64(address),
        resolved: false,
    }
}

pub(super) fn event_matches_target(pid: u32, target_pid: u32) -> bool {
    pid == target_pid
}

pub(super) fn hex64(value: u64) -> String {
    format!("0x{value:016x}")
}

pub(super) fn hex32(value: u32) -> String {
    format!("0x{value:08x}")
}
