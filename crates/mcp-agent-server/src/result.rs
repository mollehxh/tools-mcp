use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Map, Value};

/// Serializes a typed success into equivalent structured and JSON text content.
///
/// # Errors
///
/// Returns the serialization error when the transport-neutral DTO cannot be
/// represented as JSON.
pub fn success_result(value: &impl Serialize) -> Result<CallToolResult, serde_json::Error> {
    serde_json::to_value(value).map(CallToolResult::structured)
}

#[must_use]
pub fn error_result(code: &str, message: &str, details: Option<Value>) -> CallToolResult {
    let mut value = Map::from_iter([
        ("error".to_string(), Value::String(code.to_string())),
        ("message".to_string(), Value::String(message.to_string())),
    ]);
    if let Some(details) = details {
        value.insert("details".to_string(), details);
    }
    CallToolResult::structured_error(Value::Object(value))
}

#[must_use]
pub fn invalid_arguments(message: impl AsRef<str>) -> CallToolResult {
    error_result("invalid_arguments", message.as_ref(), None)
}

#[must_use]
pub fn internal_serialization_error() -> CallToolResult {
    error_result(
        "result_serialization",
        "the tool result could not be serialized",
        None,
    )
}
