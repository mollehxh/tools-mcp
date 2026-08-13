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
        (
            "message".to_string(),
            Value::String(sanitize_model_text(message)),
        ),
    ]);
    if let Some(mut details) = details {
        sanitize_model_value(&mut details);
        value.insert("details".to_string(), details);
    }
    CallToolResult::structured_error(Value::Object(value))
}

fn sanitize_model_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = sanitize_model_text(text),
        Value::Array(values) => values.iter_mut().for_each(sanitize_model_value),
        Value::Object(values) => values.values_mut().for_each(sanitize_model_value),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn sanitize_model_text(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(redact_path_token)
        .collect()
}

fn redact_path_token(segment: &str) -> String {
    let token_len = segment.trim_end_matches(char::is_whitespace).len();
    let (token, whitespace) = segment.split_at(token_len);
    if token.contains("://") {
        return segment.to_string();
    }

    let Some(path_start) = likely_absolute_path_start(token) else {
        return segment.to_string();
    };
    let path_end = token
        .trim_end_matches([':', ',', ';', '.', ')', ']', '}', '\'', '"'])
        .len();
    if path_end <= path_start {
        return segment.to_string();
    }

    format!(
        "{}[redacted-path]{}{}",
        &token[..path_start],
        &token[path_end..],
        whitespace
    )
}

fn likely_absolute_path_start(token: &str) -> Option<usize> {
    let bytes = token.as_bytes();
    for (index, character) in token.char_indices() {
        let byte = character as u32;
        let boundary = index == 0
            || token[..index]
                .chars()
                .next_back()
                .is_some_and(|character| !character.is_alphanumeric() && character != '_');
        if !boundary {
            continue;
        }

        if byte == u32::from(b'/') && token != "/mcp" {
            return Some(index);
        }
        if index + 2 < bytes.len()
            && character.is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'/' | b'\\')
        {
            return Some(index);
        }
        if index + 1 < bytes.len()
            && matches!(character, '/' | '\\')
            && bytes[index + 1] == u8::try_from(byte).expect("ASCII path separator")
        {
            return Some(index);
        }
    }
    None
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
