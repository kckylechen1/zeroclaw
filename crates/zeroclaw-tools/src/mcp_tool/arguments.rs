//! Bounded, value-free recovery for unambiguous MCP argument shape errors.
//!
//! This is not a JSON Schema validator. The server owns full validation;
//! references, compositions, nested constraints and business rules stay there.

use serde_json::Value;

const MAX_HINT_FIELDS: usize = 8;
const MAX_FIELD_CHARS: usize = 48;

fn simple_schema(schema: &Value) -> bool {
    schema.is_object()
        && ![
            "$ref",
            "$dynamicRef",
            "allOf",
            "anyOf",
            "oneOf",
            "not",
            "if",
            "then",
            "else",
        ]
        .iter()
        .any(|key| schema.get(key).is_some())
}

fn declared_types(schema: &Value) -> Option<Vec<&str>> {
    if !simple_schema(schema) {
        return None;
    }
    let candidates = match schema.get("type")? {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    let mut types = Vec::new();
    for kind in candidates {
        if !matches!(
            kind,
            "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
        ) {
            return None;
        }
        if !types.contains(&kind) {
            types.push(kind);
        }
    }
    if !types.is_empty()
        && schema.get("nullable") == Some(&Value::Bool(true))
        && !types.contains(&"null")
    {
        types.push("null");
    }
    (!types.is_empty()).then_some(types)
}

fn has_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        "string" => value.is_string(),
        _ => true,
    }
}

fn type_mismatch(schema: &Value, value: &Value) -> Option<String> {
    // Some MCP schemas use OpenAPI's nullable extension. Let the server decide
    // its dialect rather than reject a potentially valid null locally.
    if value.is_null() && schema.get("nullable") == Some(&Value::Bool(true)) {
        return None;
    }
    let types = declared_types(schema)?;
    (!types.iter().any(|kind| has_type(value, kind))).then(|| types.join(" | "))
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) if has_type(value, "integer") => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Only schema-owned identifiers are rendered, never an input key or value.
/// Keep names on one line and bound even a malicious server's schema metadata.
fn field_name(name: &str) -> String {
    let mut bounded: String = name
        .chars()
        .take(MAX_FIELD_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '?'
            }
        })
        .collect();
    if name.chars().nth(MAX_FIELD_CHARS).is_some() {
        bounded.push('…');
    }
    format!("\"{bounded}\"")
}

fn signature(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return declared_types(schema)
            .map(|types| types.join(" | "))
            .unwrap_or_else(|| "schema".into());
    };
    let required = schema.get("required").and_then(Value::as_array);
    let mut fields: Vec<String> = properties
        .iter()
        .take(MAX_HINT_FIELDS)
        .map(|(name, property)| {
            let optional = if required.is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str() == Some(name.as_str()))
            }) {
                ""
            } else {
                "?"
            };
            let kind = declared_types(property)
                .map(|types| types.join(" | "))
                .unwrap_or_else(|| "schema".into());
            format!("{}{optional}: {kind}", field_name(name))
        })
        .collect();
    if properties.len() > MAX_HINT_FIELDS {
        fields.push("…".into());
    }
    format!("{{{}}}", fields.join(", "))
}

fn hint(schema: &Value, problem: &str) -> String {
    crate::i18n::get_required_tool_string_with_args(
        "tool-mcp-arguments-invalid",
        &[("problem", problem), ("signature", &signature(schema))],
    )
}

pub(super) fn server_rejection(schema: &Value) -> String {
    hint(
        schema,
        &crate::i18n::get_required_tool_string("tool-mcp-arguments-server-rejected"),
    )
}

pub(super) fn shape_error(schema: &Value, args: &Value) -> Option<String> {
    if !simple_schema(schema) {
        return None;
    }
    if let Some(expected) = type_mismatch(schema, args) {
        let problem = crate::i18n::get_required_tool_string_with_args(
            "tool-mcp-arguments-root-type",
            &[("expected", &expected), ("actual", value_kind(args))],
        );
        return Some(hint(schema, &problem));
    }
    let args = args.as_object()?;
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !args.contains_key(name) {
                let problem = crate::i18n::get_required_tool_string_with_args(
                    "tool-mcp-arguments-missing",
                    &[("field", &field_name(name))],
                );
                return Some(hint(schema, &problem));
            }
        }
    }
    for (name, property) in schema.get("properties").and_then(Value::as_object)? {
        if let Some(value) = args.get(name)
            && let Some(expected) = type_mismatch(property, value)
        {
            let problem = crate::i18n::get_required_tool_string_with_args(
                "tool-mcp-arguments-field-type",
                &[
                    ("field", &field_name(name)),
                    ("expected", &expected),
                    ("actual", value_kind(value)),
                ],
            );
            return Some(hint(schema, &problem));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_and_wrong_type_hints_include_schema_but_no_values() {
        let schema = json!({"type": "object", "properties": {
            "path": {"type": "string", "description": "server-controlled prose"},
            "limit": {"type": "integer"}, "api_key": {"type": "string"}
        }, "required": ["path"]});
        let missing = shape_error(&schema, &json!({"api_key": "private-input"})).unwrap();
        assert!(missing.contains("path"));
        assert!(missing.contains("string"));
        assert!(!missing.contains("private-input"));
        assert!(!missing.contains("server-controlled prose"));
        let wrong = shape_error(
            &schema,
            &json!({"path": "private-path", "limit": "private-input"}),
        )
        .unwrap();
        assert!(wrong.contains("limit"));
        assert!(wrong.contains("integer"));
        assert!(!wrong.contains("private-path"));
        assert!(!wrong.contains("private-input"));
    }

    #[test]
    fn correct_values_nullable_and_undeclared_types_are_not_rejected() {
        let schema = json!({"type": "object", "properties": {
            "count": {"type": "integer"},
            "maybe": {"type": ["string", "null"]},
            "nullable": {"type": "string", "nullable": true},
            "opaque": {"description": "server decides"}
        }});
        assert!(
            shape_error(
                &schema,
                &json!({"count": 2.0, "maybe": null, "nullable": null, "opaque": [1]})
            )
            .is_none()
        );
        assert!(shape_error(&json!({}), &json!(["anything"])).is_none());
    }

    #[test]
    fn references_compositions_and_nested_constraints_stay_with_server() {
        for keyword in [
            "$ref",
            "$dynamicRef",
            "allOf",
            "anyOf",
            "oneOf",
            "not",
            "if",
        ] {
            let mut schema = json!({"type": "string"});
            schema[keyword] = json!([]);
            assert!(shape_error(&schema, &json!(42)).is_none(), "{keyword}");
            let wrapper = json!({"type": "object", "properties": {"value": schema}});
            assert!(
                shape_error(&wrapper, &json!({"value": 42})).is_none(),
                "{keyword}"
            );
        }
        let schema = json!({"type": "object", "properties": {"nested": {"type": "object", "required": ["inner"]}}});
        assert!(shape_error(&schema, &json!({"nested": {}})).is_none());
    }

    #[test]
    fn hint_is_bounded_and_omits_enum_defaults_descriptions_and_input_keys() {
        let mut properties = serde_json::Map::new();
        for index in 0..100 {
            properties.insert(
                format!("field{index:03}-{}", "x".repeat(1000)),
                json!({
                    "type": "string", "enum": ["private-enum"], "default": "private-default",
                    "description": "private-description"
                }),
            );
        }
        let schema = json!({"type": "object", "properties": properties});
        let output = server_rejection(&schema);
        assert!(output.len() < 1200, "{}", output.len());
        assert!(!output.contains("field008"));
        assert!(!output.contains("private-"));
        assert!(!field_name("bad\nname<script>").contains('\n'));
    }
}
