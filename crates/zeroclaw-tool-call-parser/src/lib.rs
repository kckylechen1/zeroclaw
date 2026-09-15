//! Tool call parsing for LLM responses.

use regex::Regex;
use std::{collections::HashSet, sync::LazyLock};

/// A single parsed tool call extracted from LLM output.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
    pub tool_call_id: Option<String>,
}

/// Internal tool protocol envelope variants that must not be treated as
/// user-visible channel text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProtocolEnvelopeKind {
    ToolCalls,
    ToolCallsAlias,
    FunctionCall,
    ToolResult,
    ResponsesFunctionCall,
    TaggedToolCall,
}

fn parse_arguments_value(raw: Option<&serde_json::Value>) -> serde_json::Value {
    let initial = match raw {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        Some(value) => value.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    unwrap_nested_json_strings(initial)
}

/// Recursively unwrap stringified JSON objects/arrays nested inside tool arguments.
/// Why: Gemini (and some other model_providers) sometimes double-encode nested object/array
/// parameters as JSON strings inside the outer arguments payload, which breaks tools
/// that expect `Value::Object` / `Value::Array` at those positions.
fn unwrap_nested_json_strings(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k, unwrap_nested_json_strings(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(unwrap_nested_json_strings).collect())
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(parsed) => unwrap_nested_json_strings(parsed),
                    Err(_) => serde_json::Value::String(s),
                }
            } else {
                serde_json::Value::String(s)
            }
        }
        other => other,
    }
}

fn parse_tool_call_id(
    root: &serde_json::Value,
    function: Option<&serde_json::Value>,
) -> Option<String> {
    function
        .and_then(|func| func.get("id"))
        .or_else(|| root.get("id"))
        .or_else(|| root.get("tool_call_id"))
        .or_else(|| root.get("call_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

pub fn canonicalize_json_for_tool_signature(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort_unstable();
            let mut ordered = serde_json::Map::new();
            for key in keys {
                if let Some(child) = map.get(&key) {
                    ordered.insert(key, canonicalize_json_for_tool_signature(child));
                }
            }
            serde_json::Value::Object(ordered)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(canonicalize_json_for_tool_signature)
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn parse_tool_call_value(value: &serde_json::Value) -> Option<ParsedToolCall> {
    if let Some(function) = value.get("function") {
        let tool_call_id = parse_tool_call_id(value, Some(function));
        let raw_name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let name = map_tool_name_alias(raw_name).to_string();
        if !name.is_empty() {
            let arguments = parse_arguments_value(
                function
                    .get("arguments")
                    .or_else(|| function.get("parameters")),
            );
            return Some(ParsedToolCall {
                name,
                arguments,
                tool_call_id,
            });
        }
    }

    let tool_call_id = parse_tool_call_id(value, None);
    let raw_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let name = map_tool_name_alias(raw_name).to_string();

    if name.is_empty() {
        return None;
    }

    let arguments =
        parse_arguments_value(value.get("arguments").or_else(|| value.get("parameters")));
    Some(ParsedToolCall {
        name,
        arguments,
        tool_call_id,
    })
}

fn parse_tool_calls_from_json_value(value: &serde_json::Value) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    if let Some(tool_calls) = value.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(parsed) = parse_tool_call_value(call) {
                calls.push(parsed);
            }
        }

        if !calls.is_empty() {
            return calls;
        }
    }

    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = parse_tool_call_value(item) {
                calls.push(parsed);
            }
        }
        return calls;
    }

    if let Some(parsed) = parse_tool_call_value(value) {
        calls.push(parsed);
    }

    calls
}

fn has_non_empty_string(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn has_arguments_signal(value: &serde_json::Value) -> bool {
    value.get("arguments").is_some() || value.get("parameters").is_some()
}

fn looks_like_tool_call_object(value: &serde_json::Value) -> bool {
    if let Some(function) = value.get("function").and_then(serde_json::Value::as_object) {
        let function = serde_json::Value::Object(function.clone());
        return has_non_empty_string(&function, "name") && has_arguments_signal(&function);
    }

    has_non_empty_string(value, "name") && has_arguments_signal(value)
}

fn tool_call_array_has_protocol_shape(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| !items.is_empty() && items.iter().any(looks_like_tool_call_object))
}

fn has_tool_protocol_object_signal(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let has_args = has_arguments_signal(value);
    let has_call_id = has_non_empty_string(value, "id")
        || has_non_empty_string(value, "call_id")
        || has_non_empty_string(value, "tool_call_id");

    object
        .get("function")
        .and_then(serde_json::Value::as_object)
        .is_some()
        || (has_non_empty_string(value, "name") && has_args)
        || (has_args && has_call_id)
}

fn tool_call_array_has_malformed_protocol_signal(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| !items.is_empty() && items.iter().any(has_tool_protocol_object_signal))
}

fn classify_tool_protocol_json_value(
    value: &serde_json::Value,
) -> Option<ToolProtocolEnvelopeKind> {
    if value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|ty| ty == "function_call")
        && has_non_empty_string(value, "name")
        && (has_arguments_signal(value) || has_non_empty_string(value, "call_id"))
    {
        return Some(ToolProtocolEnvelopeKind::ResponsesFunctionCall);
    }

    if tool_call_array_has_protocol_shape(value, "tool_calls") {
        return Some(ToolProtocolEnvelopeKind::ToolCalls);
    }

    if tool_call_array_has_protocol_shape(value, "toolcalls") {
        return Some(ToolProtocolEnvelopeKind::ToolCallsAlias);
    }

    if value
        .get("function_call")
        .is_some_and(looks_like_tool_call_object)
    {
        return Some(ToolProtocolEnvelopeKind::FunctionCall);
    }

    if has_non_empty_string(value, "tool_call_id")
        && (value.get("content").is_some()
            || value.get("result").is_some()
            || value.get("output").is_some())
    {
        return Some(ToolProtocolEnvelopeKind::ToolResult);
    }

    None
}

fn json_value_mentions_known_tool(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
) -> bool {
    if known_tool_names.is_empty() {
        return false;
    }

    let Some(object) = value.as_object() else {
        return value.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| json_value_mentions_known_tool(item, known_tool_names))
        });
    };

    let name_matches = |candidate: Option<&serde_json::Value>| {
        candidate
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .is_some_and(|name| known_tool_names.contains(&name.to_ascii_lowercase()))
    };

    if name_matches(object.get("name")) {
        return true;
    }

    if let Some(function) = object
        .get("function")
        .and_then(serde_json::Value::as_object)
    {
        let function = serde_json::Value::Object(function.clone());
        if json_value_mentions_known_tool(&function, known_tool_names) {
            return true;
        }
    }

    if let Some(function_call) = object.get("function_call")
        && json_value_mentions_known_tool(function_call, known_tool_names)
    {
        return true;
    }

    ["tool_calls", "toolcalls"].iter().any(|key| {
        object
            .get(*key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| json_value_mentions_known_tool(item, known_tool_names))
            })
    })
}

pub fn tool_protocol_envelope_mentions_known_tool(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    if known_tool_names.is_empty() {
        return false;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return tool_protocol_envelope_mentions_known_tool(body, known_tool_names);
    }

    if starts_with_tool_protocol_tag_or_fence(trimmed) || contains_tool_protocol_tag_marker(trimmed)
    {
        let (_, calls) = parse_tool_calls(trimmed);
        if calls
            .iter()
            .any(|call| known_tool_names.contains(&call.name.to_ascii_lowercase()))
        {
            return true;
        }
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .is_ok_and(|value| json_value_mentions_known_tool(&value, known_tool_names))
}

fn has_malformed_tool_protocol_json_signal(value: &serde_json::Value) -> bool {
    // Empty `tool_calls: []` is a valid strict-provider compatibility case;
    // similar business JSON must also carry protocol-shaped fields before it
    // is withheld from user-visible output.
    tool_call_array_has_malformed_protocol_signal(value, "tool_calls")
        || tool_call_array_has_malformed_protocol_signal(value, "toolcalls")
        || value
            .get("function_call")
            .is_some_and(has_tool_protocol_object_signal)
        || (value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|ty| ty == "function_call")
            && (has_non_empty_string(value, "name")
                || has_non_empty_string(value, "call_id")
                || has_arguments_signal(value)))
        || (has_non_empty_string(value, "tool_call_id")
            && (value.get("content").is_some()
                || value.get("result").is_some()
                || value.get("output").is_some()))
}

fn starts_with_tool_protocol_tag_or_fence(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("<tool_call")
        || lower.starts_with("<toolcall")
        || lower.starts_with("<tool-call")
        // `<tools>` only, not a `<tools` prefix: the prefix would also swallow
        // `<toolsomething>`, and unlike the aliases above this tag has a second
        // legitimate meaning (the Hermes tool *declaration* block), so it is
        // matched exactly and left to the example-guard below.
        || lower.starts_with("<tools>")
        || lower.starts_with("<invoke")
        || lower.starts_with("<functioncall")
        || lower.starts_with("<function_call")
        || starts_with_tool_protocol_fence_lower(&lower)
        || lower.starts_with("[tool_call]")
}

fn starts_with_tool_protocol_fence(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    starts_with_tool_protocol_fence_lower(&lower)
}

fn starts_with_tool_protocol_fence_lower(lower: &str) -> bool {
    lower.starts_with("```tool_call")
        || lower.starts_with("```toolcall")
        || lower.starts_with("```tool-call")
        || lower.starts_with("```invoke")
        || starts_with_tool_name_fence_lower(lower)
}

fn starts_with_tool_name_fence_lower(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("```tool") else {
        return false;
    };
    matches!(rest.chars().next(), Some(c) if c.is_whitespace() && c != '\n' && c != '\r')
}

fn contains_tool_protocol_tag_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("<tool_call")
        || lower.contains("<toolcall")
        || lower.contains("<tool-call")
        || lower.contains("<tools>")
        || lower.contains("<invoke")
        || lower.contains("<functioncall")
        || lower.contains("<function_call")
        || lower.contains("```tool_call")
        || lower.contains("```toolcall")
        || lower.contains("```tool-call")
        || lower.contains("```invoke")
        || lower.contains("```tool ")
        || lower.contains("[tool_call]")
}

pub fn looks_like_tool_protocol_example(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if let Some((body, visible_text)) = leading_json_fence_body_and_trailing_text(trimmed)
        && classify_tool_protocol_envelope(body).is_some()
        && has_example_context(visible_text)
    {
        return true;
    }

    if starts_with_tool_protocol_fence(trimmed) || contains_tool_protocol_tag_marker(trimmed) {
        let (visible_text, calls) = parse_tool_calls(trimmed);
        if !calls.is_empty() && has_example_context(&visible_text) {
            return true;
        }
    }

    false
}

fn has_example_context(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("example")
        || lower.contains("sample")
        || lower.contains("示例")
        // Common Chinese "for example" / "sample" markers. We keep this list
        // intentionally small to avoid accidentally exempting real protocol leaks.
        || lower.contains("例如")
        || lower.contains("比如")
        || lower.contains("举例")
        || lower.contains("例子")
        || lower.contains("比方说")
        || lower.contains("譬如")
}

fn leading_json_fence_body_and_trailing_text(trimmed: &str) -> Option<(&str, &str)> {
    let rest = trimmed.strip_prefix("```")?;
    let first_newline = rest.find('\n')?;
    let language = rest[..first_newline].trim().trim_end_matches('\r');
    if !language.eq_ignore_ascii_case("json") {
        return None;
    }

    let body_with_close = &rest[first_newline + 1..];
    let close_start = body_with_close.find("```")?;
    let body = body_with_close[..close_start].trim();
    let trailing = body_with_close[close_start + 3..].trim();
    (!body.is_empty() && !trailing.is_empty()).then_some((body, trailing))
}

pub fn contains_tool_protocol_tag_call(text: &str) -> bool {
    if !contains_tool_protocol_tag_marker(text) || looks_like_tool_protocol_example(text) {
        return false;
    }

    let (_, calls) = parse_tool_calls(text);
    !calls.is_empty()
}

fn classify_tagged_tool_protocol_envelope(text: &str) -> Option<ToolProtocolEnvelopeKind> {
    if !starts_with_tool_protocol_tag_or_fence(text) {
        return None;
    }
    if looks_like_tool_protocol_example(text) {
        return None;
    }

    let is_fence = starts_with_tool_protocol_fence(text);
    let (visible_text, calls) = parse_tool_calls(text);
    (!calls.is_empty() && (is_fence || visible_text.trim().is_empty()))
        .then_some(ToolProtocolEnvelopeKind::TaggedToolCall)
}

fn looks_like_malformed_tagged_tool_protocol_envelope(text: &str) -> bool {
    if !starts_with_tool_protocol_tag_or_fence(text) {
        return false;
    }
    if looks_like_tool_protocol_example(text) {
        return false;
    }

    let (visible_text, calls) = parse_tool_calls(text);
    if !calls.is_empty() || !visible_text.trim().is_empty() {
        return false;
    }

    let lower = text.to_ascii_lowercase();
    lower.contains("arguments")
        || lower.contains("parameters")
        || lower.contains("function")
        || lower.contains("name")
        || lower.contains("call_id")
        || lower.contains("tool_call_id")
}

/// JSON keys naming a tool-call container. Business JSON does not carry these.
const TOOL_PROTOCOL_CONTAINER_KEYS: [&str; 3] =
    ["\"tool_calls\"", "\"toolcalls\"", "\"function_call\""];

/// JSON keys carrying a tool call's correlation id.
const TOOL_PROTOCOL_CALL_ID_KEYS: [&str; 2] = ["\"call_id\"", "\"tool_call_id\""];

/// Every key that identifies a payload as tool protocol on its own.
fn tool_protocol_json_identifying_keys() -> impl Iterator<Item = &'static str> {
    TOOL_PROTOCOL_CONTAINER_KEYS
        .into_iter()
        .chain(TOOL_PROTOCOL_CALL_ID_KEYS)
}

fn has_malformed_tool_protocol_text_signal(text: &str) -> bool {
    let trimmed = text.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if !json_like {
        return false;
    }

    // Malformed text cannot be parsed into a Value, so keep the tool-result
    // signal close to the valid-envelope shape to avoid business JSON false positives.
    let has_tool_result_shape = text.contains("\"tool_call_id\"")
        && (text.contains("\"content\"")
            || text.contains("\"result\"")
            || text.contains("\"output\""));
    let has_protocol_container = TOOL_PROTOCOL_CONTAINER_KEYS
        .iter()
        .any(|key| text.contains(key));
    let has_arguments = text.contains("\"arguments\"") || text.contains("\"parameters\"");
    let has_call_id = TOOL_PROTOCOL_CALL_ID_KEYS
        .iter()
        .any(|key| text.contains(key));

    has_tool_result_shape || (has_protocol_container && has_arguments && has_call_id)
}

/// Whether `text` is a tool-protocol JSON payload that has not finished
/// arriving.
///
/// The completed-envelope classifiers need the whole value: they parse it, or
/// they look for a corroborating second key. A streaming consumer cannot wait
/// for either — the frame it is deciding about is on screen now, and the key
/// that gives the payload away may be the only one that has arrived. So this
/// deliberately trips on a *single* protocol key.
///
/// Being eager is the safe direction here and not in the completed case: a
/// held-back partial costs one frame and is re-rendered by the next delta,
/// whereas a rendered protocol envelope stays visible until the turn ends, or
/// indefinitely if the turn fails first. `false` for anything that already
/// parses as a complete JSON value, which the ordinary classifiers then judge
/// on their own terms.
pub fn looks_like_incomplete_tool_protocol_json(text: &str) -> bool {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if !json_like {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_incomplete_tool_protocol_json(body);
    }

    // A complete value is not this function's business.
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return false;
    }

    tool_protocol_json_identifying_keys().any(|key| trimmed.contains(key))
}

fn malformed_text_mentions_known_tool(text: &str, known_tool_names: &HashSet<String>) -> bool {
    if known_tool_names.is_empty() {
        return false;
    }

    static JSON_NAME_FIELD_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""name"\s*:\s*"([^"]+)""#).expect("JSON_NAME_FIELD_RE regex must compile")
    });

    JSON_NAME_FIELD_RE.captures_iter(text).any(|cap| {
        cap.get(1)
            .map(|name| name.as_str().trim().to_ascii_lowercase())
            .is_some_and(|name| known_tool_names.contains(&name))
    })
}

fn has_malformed_tool_protocol_text_signal_for_known_tools(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    if has_malformed_tool_protocol_text_signal(text) {
        return true;
    }

    let trimmed = text.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if !json_like {
        return false;
    }

    let has_protocol_container = text.contains("\"tool_calls\"")
        || text.contains("\"toolcalls\"")
        || text.contains("\"function_call\"");
    let has_arguments = text.contains("\"arguments\"") || text.contains("\"parameters\"");

    has_protocol_container
        && has_arguments
        && malformed_text_mentions_known_tool(text, known_tool_names)
}

fn json_fence_body(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("```")?;
    let first_newline = rest.find('\n')?;
    let language = rest[..first_newline].trim().trim_end_matches('\r');
    if !language.eq_ignore_ascii_case("json") {
        return None;
    }

    let body_with_close = &rest[first_newline + 1..];
    let close_start = body_with_close.rfind("```")?;
    if !body_with_close[close_start + 3..].trim().is_empty() {
        return None;
    }
    Some(body_with_close[..close_start].trim())
}

pub fn classify_tool_protocol_envelope(text: &str) -> Option<ToolProtocolEnvelopeKind> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(kind) = classify_tagged_tool_protocol_envelope(trimmed) {
        return Some(kind);
    }

    if let Some(body) = json_fence_body(trimmed) {
        return classify_tool_protocol_envelope(body);
    }

    let value = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
    classify_tool_protocol_json_value(&value)
}

pub fn looks_like_tool_protocol_envelope(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if classify_tool_protocol_envelope(trimmed).is_some() {
        return true;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_tool_protocol_envelope(body);
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .is_ok_and(|value| has_malformed_tool_protocol_json_signal(&value))
}

pub fn looks_like_malformed_tool_protocol_envelope(text: &str) -> bool {
    let trimmed = text.trim();
    if looks_like_tool_protocol_example(trimmed) {
        return false;
    }

    if looks_like_malformed_tagged_tool_protocol_envelope(trimmed) {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if trimmed.is_empty() || !json_like {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_malformed_tool_protocol_envelope(body);
    }

    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return false;
    }

    has_malformed_tool_protocol_text_signal(trimmed)
}

pub fn looks_like_malformed_tool_protocol_envelope_for_known_tools(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    let trimmed = text.trim();
    if looks_like_tool_protocol_example(trimmed) {
        return false;
    }

    if looks_like_malformed_tool_protocol_envelope(trimmed) {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if trimmed.is_empty() || !json_like {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_malformed_tool_protocol_envelope_for_known_tools(body, known_tool_names);
    }

    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return false;
    }

    has_malformed_tool_protocol_text_signal_for_known_tools(trimmed, known_tool_names)
}

fn is_xml_meta_tag(tag: &str) -> bool {
    let normalized = tag.to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "tool_call"
            | "toolcall"
            | "tool-call"
            | "invoke"
            | "thinking"
            | "thought"
            | "analysis"
            | "reasoning"
            | "reflection"
    )
}

/// Match opening XML tags: `<tag_name>`.  Does NOT use backreferences.
static XML_OPEN_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<([a-zA-Z_][a-zA-Z0-9_-]*)>").expect("XML_OPEN_TAG_RE regex must compile")
});

/// MiniMax XML invoke format:
/// `<invoke name="shell"><parameter name="command">pwd</parameter></invoke>`
static MINIMAX_INVOKE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<invoke\b[^>]*\bname\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*>(.*?)</invoke>"#)
        .expect("MINIMAX_INVOKE_RE regex must compile")
});

static MINIMAX_PARAMETER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<parameter\b[^>]*\bname\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*>(.*?)</parameter>"#,
    )
    .expect("MINIMAX_PARAMETER_RE regex must compile")
});

/// Extracts all `<tag>…</tag>` pairs from `input`, returning `(tag_name, inner_content)`.
/// Handles matching closing tags without regex backreferences.
fn extract_xml_pairs(input: &str) -> Vec<(&str, &str)> {
    let mut results = Vec::new();
    let mut search_start = 0;
    while let Some(open_cap) = XML_OPEN_TAG_RE.captures(&input[search_start..]) {
        let full_open = open_cap.get(0).unwrap();
        let tag_name = open_cap.get(1).unwrap().as_str();
        let open_end = search_start + full_open.end();

        let closing_tag = format!("</{tag_name}>");
        if let Some(close_pos) = input[open_end..].find(&closing_tag) {
            let inner = &input[open_end..open_end + close_pos];
            results.push((tag_name, inner.trim()));
            search_start = open_end + close_pos + closing_tag.len();
        } else {
            search_start = open_end;
        }
    }
    results
}

/// Parse XML-style tool calls in `<tool_call>` bodies.
/// Supports both nested argument tags and JSON argument payloads:
/// - `<memory_recall><query>...</query></memory_recall>`
/// - `<shell>{"command":"pwd"}</shell>`
fn parse_xml_tool_calls(xml_content: &str) -> Option<Vec<ParsedToolCall>> {
    let mut calls = Vec::new();
    let trimmed = xml_content.trim();

    if !trimmed.starts_with('<') || !trimmed.contains('>') {
        return None;
    }

    for (tool_name_str, inner_content) in extract_xml_pairs(trimmed) {
        let tool_name = tool_name_str.to_string();
        if is_xml_meta_tag(&tool_name) {
            continue;
        }

        if inner_content.is_empty() {
            continue;
        }

        let mut args = serde_json::Map::new();

        if let Some(first_json) = extract_json_values(inner_content).into_iter().next() {
            match first_json {
                serde_json::Value::Object(object_args) => {
                    args = object_args;
                }
                other => {
                    args.insert("value".to_string(), other);
                }
            }
        } else {
            for (key_str, value) in extract_xml_pairs(inner_content) {
                let key = key_str.to_string();
                if is_xml_meta_tag(&key) {
                    continue;
                }
                if !value.is_empty() {
                    args.insert(key, serde_json::Value::String(value.to_string()));
                }
            }

            if args.is_empty() {
                args.insert(
                    "content".to_string(),
                    serde_json::Value::String(inner_content.to_string()),
                );
            }
        }

        calls.push(ParsedToolCall {
            name: tool_name,
            arguments: serde_json::Value::Object(args),
            tool_call_id: None,
        });
    }

    if calls.is_empty() { None } else { Some(calls) }
}

/// Parse MiniMax-style XML tool calls with attributed invoke/parameter tags.
fn parse_minimax_invoke_calls(response: &str) -> Option<(String, Vec<ParsedToolCall>)> {
    let mut calls = Vec::new();
    let mut text_parts = Vec::new();
    let mut last_end = 0usize;

    for cap in MINIMAX_INVOKE_RE.captures_iter(response) {
        let Some(full_match) = cap.get(0) else {
            continue;
        };

        let before = response[last_end..full_match.start()].trim();
        if !before.is_empty() {
            text_parts.push(before.to_string());
        }

        let name = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().trim())
            .filter(|v| !v.is_empty());
        let body = cap.get(3).map(|m| m.as_str()).unwrap_or("").trim();
        last_end = full_match.end();

        let Some(name) = name else {
            continue;
        };

        let mut args = serde_json::Map::new();
        for param_cap in MINIMAX_PARAMETER_RE.captures_iter(body) {
            let key = param_cap
                .get(1)
                .or_else(|| param_cap.get(2))
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            let value = param_cap
                .get(3)
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if value.is_empty() {
                continue;
            }

            let parsed = extract_json_values(value).into_iter().next();
            args.insert(
                key.to_string(),
                parsed.unwrap_or_else(|| serde_json::Value::String(value.to_string())),
            );
        }

        if args.is_empty() {
            if let Some(first_json) = extract_json_values(body).into_iter().next() {
                match first_json {
                    serde_json::Value::Object(obj) => args = obj,
                    other => {
                        args.insert("value".to_string(), other);
                    }
                }
            } else if !body.is_empty() {
                args.insert(
                    "content".to_string(),
                    serde_json::Value::String(body.to_string()),
                );
            }
        }

        calls.push(ParsedToolCall {
            name: name.to_string(),
            arguments: serde_json::Value::Object(args),
            tool_call_id: None,
        });
    }

    if calls.is_empty() {
        return None;
    }

    let after = response[last_end..].trim();
    if !after.is_empty() {
        text_parts.push(after.to_string());
    }

    let text = text_parts
        .join("\n")
        .replace("<minimax:tool_call>", "")
        .replace("</minimax:tool_call>", "")
        .replace("<minimax:toolcall>", "")
        .replace("</minimax:toolcall>", "")
        .trim()
        .to_string();

    Some((text, calls))
}

const TOOL_CALL_OPEN_TAGS: [&str; 8] = [
    "<tool_call>",
    "<tool_calls>",
    "<toolcall>",
    "<tool-call>",
    // Hermes-family models sometimes emit the tool *declaration* tag around an
    // invocation. Qwen2.5-Coder-32B does this deterministically: the payload is
    // well-formed Hermes JSON, only the wrapper is wrong.
    "<tools>",
    "<invoke>",
    "<minimax:tool_call>",
    "<minimax:toolcall>",
];

const TOOL_CALL_CLOSE_TAGS: [&str; 8] = [
    "</tool_call>",
    "</tool_calls>",
    "</toolcall>",
    "</tool-call>",
    "</tools>",
    "</invoke>",
    "</minimax:tool_call>",
    "</minimax:toolcall>",
];

fn find_first_tag<'a>(haystack: &str, tags: &'a [&'a str]) -> Option<(usize, &'a str)> {
    tags.iter()
        .filter_map(|tag| haystack.find(tag).map(|idx| (idx, *tag)))
        .min_by_key(|(idx, _)| *idx)
}

fn extract_first_json_value_with_end(input: &str) -> Option<(serde_json::Value, usize)> {
    let trimmed = input.trim_start();
    let trim_offset = input.len().saturating_sub(trimmed.len());

    for (byte_idx, ch) in trimmed.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }

        let slice = &trimmed[byte_idx..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                return Some((value, trim_offset + byte_idx + consumed));
            }
        }
    }

    None
}

fn strip_leading_close_tags(mut input: &str) -> &str {
    loop {
        let trimmed = input.trim_start();
        if !trimmed.starts_with("</") {
            return trimmed;
        }

        let Some(close_end) = trimmed.find('>') else {
            return "";
        };
        input = &trimmed[close_end + 1..];
    }
}

fn extract_json_values(input: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return values;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        values.push(value);
        return values;
    }

    let char_positions: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut idx = 0;
    while idx < char_positions.len() {
        let (byte_idx, ch) = char_positions[idx];
        if ch == '{' || ch == '[' {
            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    values.push(value);
                    let next_byte = byte_idx + consumed;
                    while idx < char_positions.len() && char_positions[idx].0 < next_byte {
                        idx += 1;
                    }
                    continue;
                }
            }
        }
        idx += 1;
    }

    values
}

fn skip_json_ws(input: &str, mut idx: usize) -> usize {
    while let Some(ch) = input[idx..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        idx += ch.len_utf8();
    }
    idx
}

fn find_json_field_value_start(input: &str, field: &str, start: usize) -> Option<usize> {
    let pattern = format!("\"{field}\"");
    let mut search_start = start;
    while let Some(relative) = input[search_start..].find(&pattern) {
        let key_start = search_start + relative;
        let after_key = key_start + pattern.len();
        let colon = skip_json_ws(input, after_key);
        if input[colon..].starts_with(':') {
            return Some(colon + 1);
        }
        search_start = after_key;
    }
    None
}

fn find_json_string_end(input: &str, quote_start: usize) -> Option<usize> {
    if !input[quote_start..].starts_with('"') {
        return None;
    }

    let mut escaped = false;
    for (relative, ch) in input[quote_start + 1..].char_indices() {
        let idx = quote_start + 1 + relative;
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some(idx),
            _ => {}
        }
    }

    None
}

fn parse_json_string_field_after(
    input: &str,
    field: &str,
    start: usize,
) -> Option<(String, usize)> {
    let value_start = skip_json_ws(input, find_json_field_value_start(input, field, start)?);
    let value_end = find_json_string_end(input, value_start)?;
    let value = serde_json::from_str::<String>(&input[value_start..=value_end]).ok()?;
    Some((value, value_end + 1))
}

// Narrow recovery for malformed file_write calls whose content string contains
// model-emitted unescaped quotes. This is deliberately not a general JSON
// repair path: content must be the final argument field and the remaining tail
// must only close the surrounding tool-call protocol envelope.
fn decode_recovered_json_string_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let mut value = 0u32;
                let mut valid = true;
                let mut consumed = String::with_capacity(4);
                for _ in 0..4 {
                    let Some(hex) = chars.next() else {
                        valid = false;
                        break;
                    };
                    consumed.push(hex);
                    if let Some(digit) = hex.to_digit(16) {
                        value = (value << 4) | digit;
                    } else {
                        valid = false;
                    }
                }
                if valid && consumed.len() == 4 {
                    if let Some(decoded) = char::from_u32(value) {
                        out.push(decoded);
                    } else {
                        out.push_str("\\u");
                        out.push_str(&consumed);
                    }
                } else {
                    out.push_str("\\u");
                    out.push_str(&consumed);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}

fn file_write_content_tail_is_unambiguous(input: &str, after_quote: usize) -> bool {
    let mut idx = skip_json_ws(input, after_quote);
    if !input[idx..].starts_with('}') {
        return false;
    }
    idx += '}'.len_utf8();
    idx = skip_json_ws(input, idx);

    while let Some(ch) = input[idx..].chars().next() {
        match ch {
            '}' | ']' => {
                idx += ch.len_utf8();
                idx = skip_json_ws(input, idx);
            }
            _ => break,
        }
    }

    let tail = input[idx..].trim_start();
    tail.is_empty()
        || tail.starts_with("</tool_call>")
        || tail.starts_with("</tool_calls>")
        || tail.starts_with("</tools>")
        || tail.starts_with("</toolcall>")
        || tail.starts_with("</tool-call>")
        || tail.starts_with("</invoke>")
        || tail.starts_with("</minimax:tool_call>")
        || tail.starts_with("</minimax:toolcall>")
        || tail.starts_with("```")
}

fn file_write_content_quote_starts_additional_final_field(input: &str, after_quote: usize) -> bool {
    let mut idx = skip_json_ws(input, after_quote);
    if !input[idx..].starts_with(',') {
        return false;
    }

    idx += ','.len_utf8();
    idx = skip_json_ws(input, idx);

    let Some(field_end) = find_json_string_end(input, idx) else {
        return false;
    };

    idx = skip_json_ws(input, field_end + 1);
    if !input[idx..].starts_with(':') {
        return false;
    }

    idx += ':'.len_utf8();
    idx = skip_json_ws(input, idx);

    let mut stream =
        serde_json::Deserializer::from_str(&input[idx..]).into_iter::<serde_json::Value>();
    let Some(Ok(_)) = stream.next() else {
        return false;
    };

    let consumed = stream.byte_offset();
    consumed > 0 && file_write_content_tail_is_unambiguous(input, idx + consumed)
}

fn parse_malformed_file_write_content_after(input: &str, start: usize) -> Option<String> {
    let value_start = skip_json_ws(input, find_json_field_value_start(input, "content", start)?);
    if !input[value_start..].starts_with('"') {
        return None;
    }

    let mut escaped = false;
    for (relative, ch) in input[value_start + 1..].char_indices() {
        let idx = value_start + 1 + relative;
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' if file_write_content_tail_is_unambiguous(input, idx + 1) => {
                let raw = &input[value_start + 1..idx];
                return Some(decode_recovered_json_string_fragment(raw));
            }
            '"' if file_write_content_quote_starts_additional_final_field(input, idx + 1) => {
                return None;
            }
            '"' => {}
            _ => {}
        }
    }

    None
}

fn parse_malformed_file_write_arguments(input: &str) -> Option<serde_json::Value> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let object_start = skip_json_ws(trimmed, 0);
    if !trimmed[object_start..].starts_with('{') {
        return None;
    }

    let (path, path_end) = parse_json_string_field_after(trimmed, "path", object_start)?;
    if path.trim().is_empty() {
        return None;
    }

    let content = parse_malformed_file_write_content_after(trimmed, path_end)?;
    Some(serde_json::json!({
        "path": path,
        "content": content,
    }))
}

fn parse_malformed_file_write_call(input: &str) -> Option<ParsedToolCall> {
    let trimmed = input.trim();
    let body = json_fence_body(trimmed).unwrap_or(trimmed).trim();
    if body.is_empty() || !(body.starts_with('{') || body.starts_with('[')) {
        return None;
    }

    let (name, name_end) = parse_json_string_field_after(body, "name", 0)?;
    if map_tool_name_alias(name.trim()) != "file_write" {
        return None;
    }

    let arguments_start = find_json_field_value_start(body, "arguments", name_end)
        .or_else(|| find_json_field_value_start(body, "parameters", name_end))?;
    let arguments = parse_malformed_file_write_arguments(&body[arguments_start..])?;

    Some(ParsedToolCall {
        name: "file_write".to_string(),
        arguments,
        tool_call_id: None,
    })
}

/// Find the end position of a JSON object by tracking balanced braces.
fn find_json_end(input: &str) -> Option<usize> {
    let trimmed = input.trim_start();
    let offset = input.len() - trimmed.len();

    if !trimmed.starts_with('{') {
        return None;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in trimmed.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset + i + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}

fn parse_xml_attribute_tool_calls(response: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    // Regex to find <invoke name="toolname">...</invoke> blocks
    static INVOKE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)<invoke\s+name="([^"]+)"[^>]*>(.*?)</invoke>"#)
            .expect("INVOKE_RE regex must compile")
    });

    // Regex to find <parameter name="paramname">value</parameter>
    static PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"<parameter\s+name="([^"]+)"[^>]*>([^<]*)</parameter>"#)
            .expect("PARAM_RE regex must compile")
    });

    for cap in INVOKE_RE.captures_iter(response) {
        let tool_name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");

        if tool_name.is_empty() {
            continue;
        }

        let mut arguments = serde_json::Map::new();

        for param_cap in PARAM_RE.captures_iter(inner) {
            let param_name = param_cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let param_value = param_cap.get(2).map(|m| m.as_str()).unwrap_or("");

            if !param_name.is_empty() {
                arguments.insert(
                    param_name.to_string(),
                    serde_json::Value::String(param_value.to_string()),
                );
            }
        }

        if !arguments.is_empty() {
            calls.push(ParsedToolCall {
                name: map_tool_name_alias(tool_name).to_string(),
                arguments: serde_json::Value::Object(arguments),
                tool_call_id: None,
            });
        }
    }

    calls
}

fn parse_perl_style_tool_calls(response: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    // Regex to find TOOL_CALL blocks - handle double closing braces }}
    // Matches both `TOOL_CALL { ... }} /TOOL_CALL` and `[TOOL_CALL]{ ... }}[/TOOL_CALL]`
    static PERL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)(?:\[TOOL_CALL\]|TOOL_CALL)\s*\{(.+?)\}\}\s*(?:\[/TOOL_CALL\]|/TOOL_CALL)")
            .expect("PERL_RE regex must compile")
    });

    // Regex to find tool => "name" in the content
    static TOOL_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"tool\s*=>\s*"([^"]+)""#).expect("TOOL_NAME_RE regex must compile")
    });

    // Regex to find args => { ... } block.
    // The closing brace is optional: in the square bracket variant [TOOL_CALL]{...}}[/TOOL_CALL]
    // the outer regex may consume the inner closing brace, so the args content may run to end of string.
    static ARGS_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)args\s*=>\s*\{(.+?)(?:\}|$)").expect("ARGS_BLOCK_RE regex must compile")
    });

    // Regex to find --key "value" pairs
    static ARGS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"--(\w+)\s+"([^"]+)""#).expect("ARGS_RE regex must compile"));

    for cap in PERL_RE.captures_iter(response) {
        let content = cap.get(1).map(|m| m.as_str()).unwrap_or("");

        // Extract tool name
        let tool_name = TOOL_NAME_RE
            .captures(content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");

        if tool_name.is_empty() {
            continue;
        }

        // Extract args block
        let args_block = ARGS_BLOCK_RE
            .captures(content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");

        let mut arguments = serde_json::Map::new();

        for arg_cap in ARGS_RE.captures_iter(args_block) {
            let key = arg_cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let value = arg_cap.get(2).map(|m| m.as_str()).unwrap_or("");

            if !key.is_empty() {
                arguments.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
        }

        if !arguments.is_empty() {
            calls.push(ParsedToolCall {
                name: map_tool_name_alias(tool_name).to_string(),
                arguments: serde_json::Value::Object(arguments),
                tool_call_id: None,
            });
        }
    }

    calls
}

fn parse_function_call_tool_calls(response: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    // Regex to find <FunctionCall> blocks
    static FUNC_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<FunctionCall>\s*(\w+)\s*<code>([^<]+)</code>\s*</FunctionCall>")
            .expect("FUNC_RE regex must compile")
    });

    for cap in FUNC_RE.captures_iter(response) {
        let tool_name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let args_text = cap.get(2).map(|m| m.as_str()).unwrap_or("");

        if tool_name.is_empty() {
            continue;
        }

        // Parse key>value pairs (e.g., path>/Users/.../file.txt)
        let mut arguments = serde_json::Map::new();
        for line in args_text.lines() {
            let line = line.trim();
            if let Some(pos) = line.find('>') {
                let key = line[..pos].trim();
                let value = line[pos + 1..].trim();
                if !key.is_empty() && !value.is_empty() {
                    arguments.insert(
                        key.to_string(),
                        serde_json::Value::String(value.to_string()),
                    );
                }
            }
        }

        if !arguments.is_empty() {
            calls.push(ParsedToolCall {
                name: map_tool_name_alias(tool_name).to_string(),
                arguments: serde_json::Value::Object(arguments),
                tool_call_id: None,
            });
        }
    }

    calls
}

/// Parse GLM-style tool calls from response text.
/// Map tool name aliases from various LLM model_providers to ZeroClaw tool names.
/// This handles variations like "fileread" -> "file_read", "bash" -> "shell", etc.
fn map_tool_name_alias(tool_name: &str) -> &str {
    let tool_name = tool_name
        .rsplit_once('.')
        .map(|(_, suffix)| suffix)
        .unwrap_or(tool_name);
    match tool_name {
        // Shell variations (including GLM aliases that map to shell)
        "shell" | "bash" | "sh" | "exec" | "command" | "cmd" | "browser_open" | "browser"
        | "web_search" => "shell",
        // Messaging variations
        "send_message" | "sendmessage" => "message_send",
        // File tool variations
        "fileread" | "file_read" | "readfile" | "read_file" | "file" => "file_read",
        "filewrite" | "file_write" | "writefile" | "write_file" => "file_write",
        "filelist" | "file_list" | "listfiles" | "list_files" => "file_list",
        // Memory variations
        "memoryrecall" | "memory_recall" | "recall" | "memrecall" => "memory_recall",
        "memorystore" | "memory_store" | "store" | "memstore" => "memory_store",
        "memoryforget" | "memory_forget" | "forget" | "memforget" => "memory_forget",
        // HTTP variations
        "http_request" | "http" | "fetch" | "curl" | "wget" => "http_request",
        _ => tool_name,
    }
}

fn build_curl_command(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }

    if url.chars().any(char::is_whitespace) {
        return None;
    }

    let escaped = url.replace('\'', r#"'"'"'"#);
    Some(format!("curl -s '{}'", escaped))
}

fn parse_glm_style_tool_calls(text: &str) -> Vec<(String, serde_json::Value, Option<String>)> {
    let mut calls = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: tool_name/param>value or tool_name/{json}
        if let Some(pos) = line.find('/') {
            let tool_part = &line[..pos];
            let rest = &line[pos + 1..];

            if tool_part.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let tool_name = map_tool_name_alias(tool_part);

                if let Some(gt_pos) = rest.find('>') {
                    let param_name = rest[..gt_pos].trim();
                    let value = rest[gt_pos + 1..].trim();

                    let arguments = match tool_name {
                        "shell" => {
                            if param_name == "url" {
                                let Some(command) = build_curl_command(value) else {
                                    continue;
                                };
                                serde_json::json!({ "command": command })
                            } else if value.starts_with("http://") || value.starts_with("https://")
                            {
                                if let Some(command) = build_curl_command(value) {
                                    serde_json::json!({ "command": command })
                                } else {
                                    serde_json::json!({ "command": value })
                                }
                            } else {
                                serde_json::json!({ "command": value })
                            }
                        }
                        "http_request" => {
                            serde_json::json!({"url": value, "method": "GET"})
                        }
                        _ => serde_json::json!({ param_name: value }),
                    };

                    calls.push((tool_name.to_string(), arguments, Some(line.to_string())));
                    continue;
                }

                if rest.starts_with('{')
                    && let Ok(json_args) = serde_json::from_str::<serde_json::Value>(rest)
                {
                    calls.push((tool_name.to_string(), json_args, Some(line.to_string())));
                }
            }
        }
    }

    calls
}

fn default_param_for_tool(tool: &str) -> &'static str {
    match tool {
        "shell" | "bash" | "sh" | "exec" | "command" | "cmd" => "command",
        // All file tools default to "path"
        "file_read" | "fileread" | "readfile" | "read_file" | "file" | "file_write"
        | "filewrite" | "writefile" | "write_file" | "file_edit" | "fileedit" | "editfile"
        | "edit_file" | "file_list" | "filelist" | "listfiles" | "list_files" => "path",
        // Memory recall/forget and web search tools all default to "query"
        "memory_recall" | "memoryrecall" | "recall" | "memrecall" | "memory_forget"
        | "memoryforget" | "forget" | "memforget" | "web_search_tool" | "web_search"
        | "websearch" | "search" => "query",
        "memory_store" | "memorystore" | "store" | "memstore" => "content",
        // HTTP and browser tools default to "url"
        "http_request" | "http" | "fetch" | "curl" | "wget" | "browser_open" | "browser" => "url",
        _ => "input",
    }
}

#[allow(clippy::question_mark)] // multi-branch if-else chain, ? does not apply
fn parse_glm_shortened_body(body: &str) -> Option<ParsedToolCall> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    let function_style = body.find('(').and_then(|open| {
        if body.ends_with(')') && open > 0 {
            Some((body[..open].trim(), body[open + 1..body.len() - 1].trim()))
        } else {
            None
        }
    });

    // Check attribute-style FIRST: `tool_name key="value" />`
    // Must come before `>` check because `/>` contains `>` and would
    // misparse the tool name in the first branch.
    let (tool_raw, value_part) = if let Some((tool, args)) = function_style {
        (tool, args)
    } else if body.contains("=\"") {
        // Attribute-style: split at first whitespace to get tool name
        let split_pos = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
        let tool = body[..split_pos].trim();
        let attrs = body[split_pos..]
            .trim()
            .trim_end_matches("/>")
            .trim_end_matches('>')
            .trim_end_matches('/')
            .trim();
        (tool, attrs)
    } else if let Some(gt_pos) = body.find('>') {
        // GLM shortened: `tool_name>value`
        let tool = body[..gt_pos].trim();
        let value = body[gt_pos + 1..].trim();
        // Strip trailing self-close markers that some models emit
        let value = value.trim_end_matches("/>").trim_end_matches('/').trim();
        (tool, value)
    } else {
        return None;
    };

    // Validate tool name: must be alphanumeric + underscore only
    let tool_raw = tool_raw.trim_end_matches(|c: char| c.is_whitespace());
    if tool_raw.is_empty() || !tool_raw.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }

    let tool_name = map_tool_name_alias(tool_raw);

    // Try attribute-style: `key="value" key2="value2"`
    if value_part.contains("=\"") {
        let mut args = serde_json::Map::new();
        // Simple attribute parser: key="value" pairs
        let mut rest = value_part;
        while let Some(eq_pos) = rest.find("=\"") {
            let key_start = rest[..eq_pos]
                .rfind(|c: char| c.is_whitespace())
                .map(|p| p + 1)
                .unwrap_or(0);
            let key = rest[key_start..eq_pos]
                .trim()
                .trim_matches(|c: char| c == ',' || c == ';');
            let after_quote = &rest[eq_pos + 2..];
            if let Some(end_quote) = after_quote.find('"') {
                let value = &after_quote[..end_quote];
                if !key.is_empty() {
                    args.insert(
                        key.to_string(),
                        serde_json::Value::String(value.to_string()),
                    );
                }
                rest = &after_quote[end_quote + 1..];
            } else {
                break;
            }
        }
        if !args.is_empty() {
            return Some(ParsedToolCall {
                name: tool_name.to_string(),
                arguments: serde_json::Value::Object(args),
                tool_call_id: None,
            });
        }
    }

    // Try YAML-style multi-line: each line is `key: value`
    if value_part.contains('\n') {
        let mut args = serde_json::Map::new();
        for line in value_part.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(colon_pos) = line.find(':') {
                let key = line[..colon_pos].trim();
                let value = line[colon_pos + 1..].trim();
                if !key.is_empty() && !value.is_empty() {
                    // Normalize boolean-like values
                    let json_value = match value {
                        "true" | "yes" => serde_json::Value::Bool(true),
                        "false" | "no" => serde_json::Value::Bool(false),
                        _ => serde_json::Value::String(value.to_string()),
                    };
                    args.insert(key.to_string(), json_value);
                }
            }
        }
        if !args.is_empty() {
            return Some(ParsedToolCall {
                name: tool_name.to_string(),
                arguments: serde_json::Value::Object(args),
                tool_call_id: None,
            });
        }
    }

    // Single-value shortened: `tool>value`
    if !value_part.is_empty() {
        let param = default_param_for_tool(tool_raw);
        let arguments = match tool_name {
            "shell" => {
                if value_part.starts_with("http://") || value_part.starts_with("https://") {
                    if let Some(cmd) = build_curl_command(value_part) {
                        serde_json::json!({ "command": cmd })
                    } else {
                        serde_json::json!({ "command": value_part })
                    }
                } else {
                    serde_json::json!({ "command": value_part })
                }
            }
            "http_request" => serde_json::json!({"url": value_part, "method": "GET"}),
            _ => serde_json::json!({ param: value_part }),
        };
        return Some(ParsedToolCall {
            name: tool_name.to_string(),
            arguments,
            tool_call_id: None,
        });
    }

    None
}

fn malformed_tool_block_event(payload_len: usize) -> ::zeroclaw_log::Event {
    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
        .with_attrs(::serde_json::json!({
            "payload_len": payload_len,
        }))
}

/// Is this JSON value an *invocation* rather than a tool *declaration*?
///
/// Only consulted for the `<tools>` wrapper, which is overloaded: in the Hermes
/// prompt format `<tools>` DECLARES the available tools, while `<tool_call>`
/// invokes one. Every other alias in [`TOOL_CALL_OPEN_TAGS`] has exactly one
/// meaning, so this narrowing does not apply to them.
///
/// The distinction cannot be left to [`has_arguments_signal`], which counts
/// `parameters` as an arguments marker: a declaration carries `name` +
/// `parameters` and is therefore indistinguishable from a call by that test.
/// A declaration is rejected here on three signals a real invocation never has
/// -- a JSON array of entries, a `description`, or a `parameters` schema in
/// place of concrete `arguments`.
fn looks_like_tools_wrapper_invocation(value: &serde_json::Value) -> bool {
    // A declaration block is an ARRAY of tool schemas. An invocation is one call.
    let serde_json::Value::Object(map) = value else {
        return false;
    };
    // `description` and `parameters` describe a tool; they never appear in a call.
    if map.contains_key("description") || map.contains_key("parameters") {
        return false;
    }
    // OpenAI-shaped `{"type":"function","function":{...}}` declarations.
    if let Some(inner) = map.get("function").and_then(serde_json::Value::as_object)
        && (inner.contains_key("description") || inner.contains_key("parameters"))
    {
        return false;
    }
    // Only `arguments` is admitted. `args` was accepted here previously, but the
    // canonical parser reads `arguments`/`parameters` and never `args`, so an
    // `args`-only body was admitted as an invocation and then dispatched with
    // EMPTY arguments -- a corrupted call rather than an inert one. Admitting a
    // shape the parser cannot honour is worse than not admitting it: if a model
    // is found to emit `args`, implement it in the canonical parser first.
    //
    // THE ADMITTED VALUE MUST BE THE EXECUTED VALUE. `parse_tool_calls_from_json_value`
    // gives precedence to a nested `function` object, and `tool_calls` can expand one
    // value into several. A body carrying BOTH a benign top level and a nested
    // envelope therefore passed this predicate on the top level while dispatching the
    // nested content:
    //
    //   {"name":"benign","arguments":{},
    //    "function":{"name":"shell","arguments":{"command":"rm -rf /tmp/x"}}}
    //
    // The discriminator has to authorize the same representation that crosses the
    // parser boundary, so an envelope member disqualifies the body outright rather
    // than being validated on a level the parser will ignore.
    if map.contains_key("function") || map.contains_key("tool_calls") {
        return false;
    }
    has_non_empty_string(value, "name") && map.contains_key("arguments")
}

/// The extent of one `<tools>` span, and the single value it is allowed to carry.
///
/// `<tools>` is the only overloaded wrapper in [`TOOL_CALL_OPEN_TAGS`]: in the
/// Hermes prompt format it DECLARES the available tools, while some models also
/// use it to wrap an invocation. Because it is overloaded, its contract is
/// narrower than every other alias:
///
/// 1. The body is delimited STRUCTURALLY -- see [`tools_span`].
/// 2. It carries exactly one canonical JSON invocation, or nothing.
/// 3. Whatever it does not carry is inert, and stays inert everywhere else.
struct ToolsSpan {
    /// Byte offset in `after_open` where the body ends.
    body_end: usize,
    /// Byte length of the close tag that terminated the span; 0 when unclosed.
    close_len: usize,
    /// The one JSON value the body carried, if the body was exactly one value.
    /// `None` means nothing in this span may ever be dispatched.
    value: Option<serde_json::Value>,
}

/// Delimit a `<tools>` span by PARSING it, never by substring search.
///
/// A textual scan for the close tag is not JSON-string aware, so tag-shaped
/// bytes inside argument content act as a delimiter: the wrapper truncates
/// mid-string, the valid call is lost, and the remainder is exposed to the
/// other recovery parsers as if the model had emitted it at top level. Tool
/// arguments routinely carry markup, so quoted tag text must never delimit.
///
/// Parsing first makes the distinction structural: bytes inside the parsed
/// value belong to the value, and only a close alias that FOLLOWS the value can
/// end the span. This holds for the matching close, a foreign close alias, and
/// no close at all -- the three ways a span can end -- so all three share one
/// rule rather than each re-deriving a boundary.
///
/// When the body is not exactly one JSON value it can never be admitted, so the
/// only remaining job is to bound the inert region. That search starts AFTER any
/// value that did parse, which keeps a quoted close from truncating the span.
/// A body that looks like JSON but does not parse gets no textual search at all:
/// a close alias may be quoted inside an unterminated string, and there is no
/// valid call in a malformed body to lose by consuming the remainder.
///
/// The FIRST close alias at or after that point ends the span; bytes beyond it
/// are outside the wrapper and are parsed normally. This is deliberate. A model
/// that echoes its declaration block and then invokes a tool is the common real
/// shape, and swallowing everything to a later close would make that invocation
/// unreachable. It also concedes nothing: content after the close is content the
/// model could have emitted with no wrapper at all, so treating it as ordinary
/// output grants no capability. What the wrapper policy owns is the body it
/// delimits -- that a declaration, an echoed prompt, or a prose example inside
/// the span never becomes a call -- not the whole remainder of the response.
fn tools_span(after_open: &str) -> ToolsSpan {
    let lead = after_open.len() - after_open.trim_start().len();
    let mut de = serde_json::Deserializer::from_str(after_open.trim_start())
        .into_iter::<serde_json::Value>();

    if let Some(Ok(value)) = de.next() {
        let body_end = lead + de.byte_offset();
        if let Some(rest) = after_open.get(body_end..) {
            let ws = rest.len() - rest.trim_start().len();
            let trailing = rest.trim_start();

            // Models mix open/close aliases, so ANY close tag can terminate the
            // wrapper. It still has to follow the value to count.
            if let Some(close) = TOOL_CALL_CLOSE_TAGS
                .iter()
                .find(|tag| trailing.starts_with(**tag))
            {
                return ToolsSpan {
                    body_end: body_end + ws,
                    close_len: close.len(),
                    value: Some(value),
                };
            }

            // Unclosed, but the body IS the complete value. Truncation of the
            // wrapper must not weaken the rule the closed paths enforce, and it
            // must not strengthen it either: a complete invocation still counts.
            if trailing.is_empty() {
                return ToolsSpan {
                    body_end: after_open.len(),
                    close_len: 0,
                    value: Some(value),
                };
            }
        }

        // A value parsed, but the body holds more than that one value (trailing
        // prose, a second value). Not admissible; bound the span from the end of
        // what parsed so quoted tag text inside it cannot act as the delimiter.
        return match find_first_tag(&after_open[body_end..], &TOOL_CALL_CLOSE_TAGS) {
            Some((idx, tag)) => ToolsSpan {
                body_end: body_end + idx,
                close_len: tag.len(),
                value: None,
            },
            None => ToolsSpan {
                body_end: after_open.len(),
                close_len: 0,
                value: None,
            },
        };
    }

    // Nothing parsed. If the body opens like JSON it is malformed JSON, and a
    // close alias could be quoted inside an unterminated string -- so do not
    // trust a textual scan to bound it. Consume the remainder instead; a
    // malformed `<tools>` body has no valid call to lose, and leaving a suffix
    // behind is exactly how refused bytes reach another executable parser.
    let trimmed = after_open.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return ToolsSpan {
            body_end: after_open.len(),
            close_len: 0,
            value: None,
        };
    }

    // Prose or a declaration block: no JSON string for a close alias to hide in,
    // so a textual bound is sound and keeps any following tags parseable.
    match find_first_tag(after_open, &TOOL_CALL_CLOSE_TAGS) {
        Some((idx, tag)) => ToolsSpan {
            body_end: idx,
            close_len: tag.len(),
            value: None,
        },
        None => ToolsSpan {
            body_end: after_open.len(),
            close_len: 0,
            value: None,
        },
    }
}

/// Does a byte RANGE overlap any `<tools>` span that was refused?
///
/// Refusing to admit a body is only half of the boundary. The other half is that
/// the refused bytes are not offered to a second executable parser. Fallbacks
/// that walk `remaining` get this for free, because the `<tools>` handler
/// advances past the span. Fallbacks that re-scan the ORIGINAL response do not,
/// and must consult this instead.
///
/// THE TEST IS OVERLAP, NOT MEMBERSHIP OF THE START OFFSET. Asking only whether
/// a match BEGINS inside a refused span leaves the boundary open from the other
/// side: a fence that opens BEFORE the span and runs through it never starts
/// inside anything, passes the check, and its body -- refused bytes included --
/// is handed to `extract_json_values`, which finds the very object the `<tools>`
/// handler rejected. Start-offset containment is not span containment.
///
/// Two ranges overlap when each begins before the other ends; empty ranges
/// cannot overlap anything.
fn range_hits_rejected_span(rejected: &[std::ops::Range<usize>], start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    rejected
        .iter()
        .any(|span| !span.is_empty() && start < span.end && span.start < end)
}

pub fn parse_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
    // Strip `<think>...</think>` blocks before parsing.  Qwen and other
    // reasoning models embed chain-of-thought inline in the response text;
    // these tags can interfere with `<tool_call>` extraction and must be
    // removed first.
    let cleaned = strip_think_tags(response);
    let response = cleaned.as_str();

    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    let mut remaining = response;
    // Byte ranges of `<tools>` spans this loop refused. Consulted by the global
    // fallbacks that re-scan `response` instead of walking `remaining`.
    let mut rejected_tools_spans: Vec<std::ops::Range<usize>> = Vec::new();

    // First, try to parse as OpenAI-style JSON response with tool_calls array
    // This handles model_providers like Minimax that return tool_calls in native JSON format
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        calls = parse_tool_calls_from_json_value(&json_value);
        if !calls.is_empty() {
            // If we found tool_calls, extract any content field as text
            if let Some(content) = json_value.get("content").and_then(|v| v.as_str())
                && !content.trim().is_empty()
            {
                text_parts.push(content.trim().to_string());
            }
            return (text_parts.join("\n"), calls);
        }
    }
    if let Some(call) = parse_malformed_file_write_call(response.trim()) {
        return (String::new(), vec![call]);
    }

    // This scan searches the WHOLE response for executable `<invoke>` syntax, so
    // running it ahead of the tag loop lets legacy markup nested inside a
    // `<tools>` wrapper execute before the body is ever classified. Gating the
    // wrapper-local sites cannot protect a scan that runs first. When a `<tools>`
    // span is present the tag loop owns the text; responses without one are
    // unaffected.
    if !response.contains("<tools>")
        && let Some((minimax_text, minimax_calls)) = parse_minimax_invoke_calls(response)
        && !minimax_calls.is_empty()
    {
        return (minimax_text, minimax_calls);
    }

    // Fall back to XML-style tool-call tag parsing.
    while let Some((start, open_tag)) = find_first_tag(remaining, &TOOL_CALL_OPEN_TAGS) {
        // Everything before the tag is text
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        // `<tools>` is handled HERE, in full, and never reaches the generic
        // recovery paths below. Those paths exist to rescue malformed output from
        // unambiguous tags: they scan through prose for JSON, retry the body
        // against XML and GLM shorthand, and treat a foreign or missing close as
        // a reason to try harder. Every one of those behaviours is wrong for an
        // overloaded tag that also declares tools, and threading a guard through
        // each of them means the guard can be forgotten at the next path added.
        // Handling the tag once, at the top, makes that structurally impossible.
        if open_tag == "<tools>" {
            let after_open = &remaining[start + open_tag.len()..];
            let span = tools_span(after_open);
            let consumed = span.body_end + span.close_len;

            // Exactly one canonical invocation is the entire admissible set.
            let admitted = span
                .value
                .as_ref()
                .filter(|value| looks_like_tools_wrapper_invocation(value))
                .map(parse_tool_calls_from_json_value)
                .filter(|parsed| !parsed.is_empty());

            if let Some(parsed) = admitted {
                calls.extend(parsed);
            } else {
                // Refused. The body becomes visible text, and the span is
                // recorded so no later parser can execute the bytes this
                // boundary just declined.
                let body = after_open[..span.body_end].trim();
                if !body.is_empty() {
                    text_parts.push(body.to_string());
                }
                let span_start = response.len() - remaining.len() + start;
                rejected_tools_spans.push(span_start..span_start + open_tag.len() + consumed);
            }

            remaining = &after_open[consumed..];
            continue;
        }

        let Some(close_tag) = (match open_tag {
            "<tool_call>" => Some("</tool_call>"),
            "<tool_calls>" => Some("</tool_calls>"),
            "<toolcall>" => Some("</toolcall>"),
            "<tool-call>" => Some("</tool-call>"),
            "<invoke>" => Some("</invoke>"),
            "<minimax:tool_call>" => Some("</minimax:tool_call>"),
            "<minimax:toolcall>" => Some("</minimax:toolcall>"),
            _ => None,
        }) else {
            break;
        };

        let after_open = &remaining[start + open_tag.len()..];
        if let Some(close_idx) = after_open.find(close_tag) {
            let inner = &after_open[..close_idx];
            let mut parsed_any = false;

            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    parsed_any = true;
                    calls.extend(parsed_calls);
                }
            }

            if !parsed_any && let Some(call) = parse_malformed_file_write_call(inner) {
                calls.push(call);
                parsed_any = true;
            }

            // If JSON parsing failed, try XML format (DeepSeek/GLM style)
            if !parsed_any && let Some(xml_calls) = parse_xml_tool_calls(inner) {
                calls.extend(xml_calls);
                parsed_any = true;
            }

            if !parsed_any {
                // GLM-style shortened body: `shell>uname -a` or `shell\ncommand: date`
                if let Some(glm_call) = parse_glm_shortened_body(inner) {
                    calls.push(glm_call);
                    parsed_any = true;
                }
            }

            if !parsed_any {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "Malformed <tool_call>: expected tool-call object in tag body (JSON/XML/GLM)"
                );
            }

            remaining = &after_open[close_idx + close_tag.len()..];
        } else {
            // Matching close tag not found — try cross-alias close tags first.
            // Models sometimes mix open/close tag aliases (e.g. <tool_call>...</invoke>).
            let mut resolved = false;
            if let Some((cross_idx, cross_tag)) = find_first_tag(after_open, &TOOL_CALL_CLOSE_TAGS)
            {
                let inner = &after_open[..cross_idx];
                let mut parsed_any = false;

                // Try JSON
                let json_values = extract_json_values(inner);
                for value in json_values {
                    let parsed_calls = parse_tool_calls_from_json_value(&value);
                    if !parsed_calls.is_empty() {
                        parsed_any = true;
                        calls.extend(parsed_calls);
                    }
                }

                if !parsed_any && let Some(call) = parse_malformed_file_write_call(inner) {
                    calls.push(call);
                    parsed_any = true;
                }

                // Try XML
                if !parsed_any && let Some(xml_calls) = parse_xml_tool_calls(inner) {
                    calls.extend(xml_calls);
                    parsed_any = true;
                }

                // Try GLM shortened body
                if !parsed_any && let Some(glm_call) = parse_glm_shortened_body(inner) {
                    calls.push(glm_call);
                    parsed_any = true;
                }

                if parsed_any {
                    remaining = &after_open[cross_idx + cross_tag.len()..];
                    resolved = true;
                }
            }

            if resolved {
                continue;
            }

            // No cross-alias close tag resolved — fall back to JSON recovery
            // from unclosed tags (brace-balancing).
            if let Some(json_end) = find_json_end(after_open)
                && let Ok(value) =
                    serde_json::from_str::<serde_json::Value>(&after_open[..json_end])
            {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    calls.extend(parsed_calls);
                    remaining = strip_leading_close_tags(&after_open[json_end..]);
                    continue;
                }
            }

            if let Some((value, consumed_end)) = extract_first_json_value_with_end(after_open) {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    calls.extend(parsed_calls);
                    remaining = strip_leading_close_tags(&after_open[consumed_end..]);
                    continue;
                }
            }

            if let Some(call) = parse_malformed_file_write_call(after_open) {
                calls.push(call);
                remaining = "";
                continue;
            }

            // Last resort: try GLM shortened body on everything after the open tag.
            // The model may have emitted `<tool_call>shell>ls` with no close tag at all.
            let glm_input = after_open.trim();
            if let Some(glm_call) = parse_glm_shortened_body(glm_input) {
                calls.push(glm_call);
                remaining = "";
                continue;
            }

            remaining = &remaining[start..];
            break;
        }
    }

    // The fallbacks below re-scan the ORIGINAL response rather than walking
    // `remaining`, so the tag loop's consume-on-reject does not reach them: a
    // `<tools>` body this parser already refused is otherwise handed straight to
    // the next executable parser, which has no notion of the wrapper policy.
    // Every match therefore has to be checked against the refused spans. The
    // fallbacks further down operate on `remaining` and are covered already.

    // If XML tags found nothing, try markdown code blocks with tool_call language.
    // Models behind OpenRouter sometimes output ```tool_call ... ``` or hybrid
    // ```tool_call ... </tool_call> instead of structured API calls or XML tags.
    if calls.is_empty() {
        static MD_TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?s)```(?:tool[_-]?call|invoke)\s*\n(.*?)(?:```|</tool[_-]?call>|</toolcall>|</invoke>|</minimax:toolcall>)",
            )
            .expect("MD_TOOL_CALL_RE regex must compile")
        });
        let mut md_text_parts: Vec<String> = Vec::new();
        let mut last_end = 0;

        for cap in MD_TOOL_CALL_RE.captures_iter(response) {
            let full_match = cap.get(0).unwrap();
            // Range-aware: a fence that OPENS before a refused span and runs
            // through it must be refused too, not just one that starts inside.
            if range_hits_rejected_span(&rejected_tools_spans, full_match.start(), full_match.end())
            {
                continue;
            }
            let before = &response[last_end..full_match.start()];
            if !before.trim().is_empty() {
                md_text_parts.push(before.trim().to_string());
            }
            let inner = &cap[1];
            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                calls.extend(parsed_calls);
            }
            if calls.is_empty()
                && let Some(call) = parse_malformed_file_write_call(inner)
            {
                calls.push(call);
            }
            last_end = full_match.end();
        }

        if !calls.is_empty() {
            let after = &response[last_end..];
            if !after.trim().is_empty() {
                md_text_parts.push(after.trim().to_string());
            }
            text_parts = md_text_parts;
            remaining = "";
        }
    }

    // Try ```tool <name> format used by some model_providers (e.g., xAI grok)
    // Example: ```tool file_write\n{"path": "...", "content": "..."}\n```
    if calls.is_empty() {
        static MD_TOOL_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?s)```tool\s+(\w+)\s*\n(.*?)(?:```|$)")
                .expect("MD_TOOL_NAME_RE regex must compile")
        });
        let mut md_text_parts: Vec<String> = Vec::new();
        let mut last_end = 0;

        for cap in MD_TOOL_NAME_RE.captures_iter(response) {
            let full_match = cap.get(0).unwrap();
            // Range-aware: a fence that OPENS before a refused span and runs
            // through it must be refused too, not just one that starts inside.
            if range_hits_rejected_span(&rejected_tools_spans, full_match.start(), full_match.end())
            {
                continue;
            }
            let before = &response[last_end..full_match.start()];
            if !before.trim().is_empty() {
                md_text_parts.push(before.trim().to_string());
            }
            let tool_name = &cap[1];
            let inner = &cap[2];

            // Try to parse the inner content as JSON arguments
            let json_values = extract_json_values(inner);
            if json_values.is_empty() {
                if map_tool_name_alias(tool_name) == "file_write"
                    && let Some(arguments) = parse_malformed_file_write_arguments(inner)
                {
                    calls.push(ParsedToolCall {
                        name: "file_write".to_string(),
                        arguments,
                        tool_call_id: None,
                    });
                } else {
                    // Log a warning if we found a tool block but couldn't parse arguments
                    ::zeroclaw_log::record!(
                        WARN,
                        malformed_tool_block_event(inner.len()),
                        "Found ```tool <name> block but could not parse JSON arguments"
                    );
                }
            } else {
                for value in json_values {
                    let arguments = if value.is_object() {
                        value
                    } else {
                        serde_json::Value::Object(serde_json::Map::new())
                    };
                    calls.push(ParsedToolCall {
                        name: tool_name.to_string(),
                        arguments,
                        tool_call_id: None,
                    });
                }
            }
            last_end = full_match.end();
        }

        if !calls.is_empty() {
            let after = &response[last_end..];
            if !after.trim().is_empty() {
                md_text_parts.push(after.trim().to_string());
            }
            text_parts = md_text_parts;
            remaining = "";
        }
    }

    if calls.is_empty() {
        let xml_calls = parse_xml_attribute_tool_calls(remaining);
        if !xml_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for call in xml_calls {
                calls.push(call);
                // Try to remove the XML from text
                if let Some(start) = cleaned_text.find("<minimax:toolcall>")
                    && let Some(end) = cleaned_text.find("</minimax:toolcall>")
                {
                    let end_pos = end + "</minimax:toolcall>".len();
                    if end_pos <= cleaned_text.len() {
                        cleaned_text =
                            format!("{}{}", &cleaned_text[..start], &cleaned_text[end_pos..]);
                    }
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    if calls.is_empty() {
        let perl_calls = parse_perl_style_tool_calls(remaining);
        if !perl_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for call in perl_calls {
                calls.push(call);
                // Try to remove the TOOL_CALL block from text
                while let Some(start) = cleaned_text.find("TOOL_CALL") {
                    if let Some(end) = cleaned_text.find("/TOOL_CALL") {
                        let end_pos = end + "/TOOL_CALL".len();
                        if end_pos <= cleaned_text.len() {
                            cleaned_text =
                                format!("{}{}", &cleaned_text[..start], &cleaned_text[end_pos..]);
                        }
                    } else {
                        break;
                    }
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // <FunctionCall>
    // file_read
    // <code>path>/Users/...</code>
    // </FunctionCall>
    if calls.is_empty() {
        let func_calls = parse_function_call_tool_calls(remaining);
        if !func_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for call in func_calls {
                calls.push(call);
                // Try to remove the FunctionCall block from text
                while let Some(start) = cleaned_text.find("<FunctionCall>") {
                    if let Some(end) = cleaned_text.find("</FunctionCall>") {
                        let end_pos = end + "</FunctionCall>".len();
                        if end_pos <= cleaned_text.len() {
                            cleaned_text =
                                format!("{}{}", &cleaned_text[..start], &cleaned_text[end_pos..]);
                        }
                    } else {
                        break;
                    }
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // GLM-style tool calls (browser_open/url>https://..., shell/command>ls, etc.)
    if calls.is_empty() {
        let glm_calls = parse_glm_style_tool_calls(remaining);
        if !glm_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for (name, args, raw) in &glm_calls {
                calls.push(ParsedToolCall {
                    name: name.clone(),
                    arguments: args.clone(),
                    tool_call_id: None,
                });
                if let Some(r) = raw {
                    cleaned_text = cleaned_text.replace(r, "");
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // Remaining text after last tool call
    if !remaining.trim().is_empty() {
        text_parts.push(remaining.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

/// Remove `<think>...</think>` blocks from model output.
/// Qwen and other reasoning models embed chain-of-thought inline in the
/// response text using `<think>` tags.  These must be removed before parsing
/// tool-call tags or displaying output.
pub fn strip_think_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        if let Some(start) = rest.find("<think>") {
            result.push_str(&rest[..start]);
            if let Some(end) = rest[start..].find("</think>") {
                rest = &rest[start + end + "</think>".len()..];
            } else {
                // Unclosed tag: drop the rest to avoid leaking partial reasoning.
                break;
            }
        } else {
            result.push_str(rest);
            break;
        }
    }
    result.trim().to_string()
}

/// Strip prompt-guided tool artifacts from visible output while preserving
/// raw model text in history for future turns.
pub fn strip_tool_result_blocks(text: &str) -> String {
    static TOOL_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<tool_result[^>]*>.*?</tool_result>")
            .expect("TOOL_RESULT_RE regex must compile")
    });
    static THINKING_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<thinking>.*?</thinking>").expect("THINKING_RE regex must compile")
    });
    static THINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<think>.*?</think>").expect("THINK_RE regex must compile")
    });
    static TOOL_RESULTS_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\[Tool results\]\s*\n?")
            .expect("TOOL_RESULTS_PREFIX_RE regex must compile")
    });
    static EXCESS_BLANK_LINES_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\n{3,}").expect("EXCESS_BLANK_LINES_RE regex must compile"));

    let result = TOOL_RESULT_RE.replace_all(text, "");
    let result = THINKING_RE.replace_all(&result, "");
    let result = THINK_RE.replace_all(&result, "");
    let result = TOOL_RESULTS_PREFIX_RE.replace_all(&result, "");
    let result = EXCESS_BLANK_LINES_RE.replace_all(result.trim(), "\n\n");

    result.trim().to_string()
}

pub fn detect_tool_call_parse_issue(
    response: &str,
    parsed_calls: &[ParsedToolCall],
) -> Option<String> {
    if !parsed_calls.is_empty() {
        return None;
    }

    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }

    if looks_like_tool_protocol_envelope(trimmed) {
        return Some(
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into(),
        );
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return has_malformed_tool_protocol_json_signal(&value).then(|| {
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into()
        });
    }

    if has_malformed_tool_protocol_text_signal(trimmed) {
        return Some(
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into(),
        );
    }

    let contains_tool_payload_marker = trimmed.contains("<tool_call")
        || trimmed.contains("<toolcall")
        || trimmed.contains("<tool-call")
        || trimmed.contains("```tool_call")
        || trimmed.contains("```toolcall")
        || trimmed.contains("```tool-call")
        || trimmed.contains("```tool file_")
        || trimmed.contains("```tool shell")
        || trimmed.contains("```tool web_")
        || trimmed.contains("```tool memory_")
        || trimmed.contains("```tool ") // Generic ```tool <name> pattern
        || trimmed.contains("TOOL_CALL")
        || trimmed.contains("[TOOL_CALL]")
        || trimmed.contains("<FunctionCall>");

    if contains_tool_payload_marker {
        if looks_like_tool_protocol_example(trimmed) {
            return None;
        }
        if contains_tool_protocol_tag_call(trimmed) {
            return Some(
                "response resembled a tool-call payload but no valid tool call could be parsed"
                    .into(),
            );
        }

        let (visible_text, recovered_calls) = parse_tool_calls(trimmed);
        if !recovered_calls.is_empty() && !visible_text.trim().is_empty() {
            return None;
        }
        if !recovered_calls.is_empty() || visible_text.trim().is_empty() {
            return Some(
                "response resembled a tool-call payload but no valid tool call could be parsed"
                    .into(),
            );
        }
    }

    if looks_like_malformed_tool_protocol_envelope(trimmed) {
        Some("response resembled a tool-call payload but no valid tool call could be parsed".into())
    } else {
        None
    }
}

pub fn build_native_assistant_history_from_parsed_calls(
    text: &str,
    tool_calls: &[ParsedToolCall],
    reasoning_content: Option<&str>,
) -> Option<String> {
    // Strict provider validators (DeepSeek V4, NVIDIA NIM, ...) reject
    // assistant messages that carry `tool_calls: []`. When there are no
    // parsed calls, return None so the caller falls through to a plain
    // text assistant message.
    if tool_calls.is_empty() {
        return None;
    }

    let calls_json = tool_calls
        .iter()
        .map(|tc| {
            Some(serde_json::json!({
                "id": tc.tool_call_id.clone()?,
                "name": tc.name,
                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".to_string()),
            }))
        })
        .collect::<Option<Vec<_>>>()?;

    let content = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text.trim().to_string())
    };

    let mut obj = serde_json::json!({
        "content": content,
        "tool_calls": calls_json,
    });

    if let Some(rc) = reasoning_content {
        obj.as_object_mut().unwrap().insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(rc.to_string()),
        );
    }

    Some(obj.to_string())
}

#[cfg(test)]
mod tests;
