use crate::{DbgFlowError, Result};
use std::path::{Component, Path};

pub fn validate_plain_text(value: &str, label: &str) -> Result<()> {
    if value
        .chars()
        .any(|ch| matches!(ch, '\0' | '\r' | '\n' | '\u{2028}' | '\u{2029}') || ch.is_control())
    {
        return Err(DbgFlowError::Backend(format!(
            "{label} contains unsupported control characters"
        )));
    }
    Ok(())
}

pub fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

pub fn split_path_text_has_parent(path: &str) -> bool {
    path.split(['\\', '/']).any(|part| part == "..")
}

pub fn path_text_has_separator(path: &str) -> bool {
    path.contains('\\') || path.contains('/')
}

pub fn is_absolute_path_text(path: &Path) -> bool {
    if path.is_absolute() {
        return true;
    }
    let text = path.as_os_str().to_string_lossy();
    let bytes = text.as_bytes();
    (bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
        && bytes[0].is_ascii_alphabetic())
        || text.starts_with("\\\\")
}
