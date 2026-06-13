//! Reverse-analysis crate boundary reserved for future `ida.*` work.
//!
//! This crate intentionally does not expose a reverse session, backend trait, or
//! MCP tool surface yet. It exists so future IDA/idalib work can grow behind a
//! separate domain boundary instead of copying debug or trace session code.

pub use dbgflow_common::{DbgFlowError, Result};
