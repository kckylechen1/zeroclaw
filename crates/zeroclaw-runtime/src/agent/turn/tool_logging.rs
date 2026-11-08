//! Tool-I/O policy at the log rendering boundary. Tool execution, model
//! history and receipts retain their original values.

use super::redact::scrub_credentials_value;
use serde_json::{Map, Value};
use zeroclaw_log::{ResolvedPolicy, ToolIoCapture};

const INPUT_FIELDS: &[&str] = &["input", "arguments"];
const OUTPUT_FIELDS: &[&str] = &["output", "result", "error", "error_reason", "reason"];

/// Keep diagnostic metadata while applying the live policy to every body
/// field used by canonical tool-call logging. Input and output each share one
/// byte budget, including error text, so duplicate fields cannot multiply the
/// configured cap. Existing field shapes survive unless truncation requires
/// a text preview; explicit metadata identifies that case.
pub(crate) fn tool_log_attributes(tool: &str, mut attributes: Value) -> Value {
    let Some(fields) = attributes.as_object_mut() else {
        return Value::Null;
    };
    let policy = zeroclaw_log::tool_io_policy();
    capture_fields(
        fields,
        tool,
        INPUT_FIELDS,
        "tool_input",
        policy.as_ref(),
        true,
    );
    capture_fields(
        fields,
        tool,
        OUTPUT_FIELDS,
        "tool_output",
        policy.as_ref(),
        false,
    );
    attributes
}

fn capture_fields(
    fields: &mut Map<String, Value>,
    tool: &str,
    body_fields: &[&str],
    metadata_prefix: &str,
    policy: Option<&ResolvedPolicy>,
    input: bool,
) {
    let mut policy = policy.cloned();
    let mut original_bytes = 0usize;
    let mut truncated = false;
    let mut captured = false;
    for &name in body_fields {
        let Some(value) = fields.remove(name) else {
            continue;
        };
        let Some(policy) = policy.as_mut() else {
            continue;
        };
        if !policy.tool_io.captures_io() || policy.is_tool_denylisted(tool) {
            continue;
        }
        let redacted = leak_scan_values(scrub_credentials_value(value));
        if redacted.is_null() {
            fields.insert(name.to_string(), redacted);
            continue;
        }
        let text = redacted
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| redacted.to_string());
        let capture = if input {
            zeroclaw_log::capture_tool_input(policy, tool, &text)
        } else {
            zeroclaw_log::capture_tool_output(policy, tool, &text)
        };
        if let Some(ToolIoCapture {
            text,
            original_bytes: bytes,
            truncated: field_truncated,
        }) = capture
        {
            captured = true;
            original_bytes = original_bytes.saturating_add(bytes);
            truncated |= field_truncated;
            policy.tool_io_truncate_bytes =
                policy.tool_io_truncate_bytes.saturating_sub(text.len());
            fields.insert(
                name.to_string(),
                if field_truncated {
                    Value::String(text)
                } else {
                    redacted
                },
            );
        }
    }
    if captured {
        fields.insert(
            format!("{metadata_prefix}_truncated"),
            Value::Bool(truncated),
        );
        fields.insert(
            format!("{metadata_prefix}_original_bytes"),
            Value::from(original_bytes),
        );
    }
}

/// The structured scrub handles credential-named keys (including composite
/// values); the existing leak scanner additionally catches bare credential
/// formats in ordinary text leaves. Neither pass touches the data path.
fn leak_scan_values(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(crate::security::scrub(&text)),
        Value::Array(values) => Value::Array(values.into_iter().map(leak_scan_values).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, leak_scan_values(value)))
                .collect(),
        ),
        other => other,
    }
}
