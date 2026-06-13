use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEvent {
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,
    pub component: String,
    pub event: String,
    pub session_id: Option<String>,
    pub backend_session_id: Option<String>,
    pub operation: Option<String>,
    pub duration_ms: Option<u64>,
    pub message: Option<String>,
    pub error: Option<String>,
    pub fields: Map<String, Value>,
}

impl LogEvent {
    pub fn new(level: LogLevel, component: impl Into<String>, event: impl Into<String>) -> Self {
        Self {
            timestamp_unix_ms: u128_to_u64(crate::time::now_unix_ms()),
            level,
            component: component.into(),
            event: event.into(),
            session_id: None,
            backend_session_id: None,
            operation: None,
            duration_ms: None,
            message: None,
            error: None,
            fields: Map::new(),
        }
    }

    pub fn session_id(mut self, session_id: impl ToString) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    pub fn backend_session_id(mut self, backend_session_id: impl Into<String>) -> Self {
        self.backend_session_id = Some(backend_session_id.into());
        self
    }

    pub fn operation(mut self, operation: impl Into<String>) -> Self {
        self.operation = Some(operation.into());
        self
    }

    pub fn duration_ms(mut self, duration_ms: u128) -> Self {
        self.duration_ms = Some(u128_to_u64(duration_ms));
        self
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn field(mut self, name: impl Into<String>, value: impl Serialize) -> Self {
        let value =
            serde_json::to_value(value).unwrap_or(Value::String("<serialize error>".into()));
        self.fields.insert(name.into(), value);
        self
    }
}

pub trait LogSink: Send + Sync {
    fn log(&self, event: LogEvent);
}

#[derive(Debug, Default)]
pub struct NoopLogSink;

impl LogSink for NoopLogSink {
    fn log(&self, _event: LogEvent) {}
}

pub fn noop_logger() -> Arc<dyn LogSink> {
    Arc::new(NoopLogSink)
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}
