//! Reverse-analysis crate boundary for `ida.*` work.
//!
//! The first IDA implementation deliberately uses runtime dynamic loading of
//! IDA DLLs. Building dbgflow must not require the IDA SDK, Clang, bindgen, or
//! an IDA installation.

pub mod ida;

pub use dbgflow_common::{DbgFlowError, Result};
