use super::registry::ToolCallError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) fn decode_arguments<T>(arguments: Value) -> std::result::Result<T, ToolCallError>
where
    T: for<'de> Deserialize<'de>,
{
    let arguments = match arguments {
        Value::Null => Value::Object(Default::default()),
        other => other,
    };
    serde_json::from_value(arguments)
        .map_err(|error| ToolCallError::invalid_request(format!("invalid tool arguments: {error}")))
}

pub(super) fn to_value<T>(value: T) -> std::result::Result<Value, ToolCallError>
where
    T: Serialize,
{
    serde_json::to_value(value)
        .map_err(|error| ToolCallError::Execution(format!("serialize tool result: {error}")))
}
