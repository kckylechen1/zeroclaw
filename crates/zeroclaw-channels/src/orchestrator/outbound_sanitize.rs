//! Outbound channel response sanitization: tool-protocol stripping, think-tag
//! removal, credential leak redaction, and empty-reply fallback.
//!
//! Extracted from `orchestrator/mod.rs` so the outbound guardrail pipeline can
//! evolve independently of channel message processing.

use std::collections::HashSet;
use std::ops::Range;

use pulldown_cmark::{Event, Options as MarkdownOptions, Parser as MarkdownParser, Tag};
use url::Url;

use zeroclaw_runtime::tools::Tool;

/// Every XML-ish element name that opens a tool-CALL envelope, longest first so
/// a prefix scan reads `<tool_call …>` as itself rather than as the shorter
/// `<tool …>`.
///
/// Canonical for the channel boundary. [`strip_tool_call_tags`] builds its
/// complete `<name>` / `</name>` pairs from this list and
/// [`truncate_at_unclosed_scratchpad_open`] derives its partial-prefix
/// inventory from it, so a dialect added here cannot be stripped from a
/// delivered message but left visible in a draft frame.
const TOOL_CALL_TAG_NAMES: [&str; 7] = [
    "function_calls",
    "function_call",
    "tool_call",
    "tool-call",
    "toolcall",
    "invoke",
    "tool",
];

/// The result envelope's element name. Kept apart from the call names because
/// the two are stripped under different conditions: the call pass is skipped
/// for an answer that is genuine `<tool_call>` documentation, while results are
/// stripped unconditionally.
const TOOL_RESULT_TAG_NAME: &str = "tool_result";

/// Every protocol element name, call and result alike, longest first.
fn tool_protocol_tag_names() -> impl Iterator<Item = &'static str> {
    // `tool_result` sorts before the bare `tool` for the same longest-first
    // reason the call list is ordered.
    TOOL_CALL_TAG_NAMES
        .into_iter()
        .take(TOOL_CALL_TAG_NAMES.len() - 1)
        .chain(std::iter::once(TOOL_RESULT_TAG_NAME))
        .chain(std::iter::once("tool"))
}

pub(crate) fn strip_tool_call_tags(message: &str) -> String {
    const TOOL_CALL_OPEN_TAGS: [&str; 7] = [
        "<function_calls>",
        "<function_call>",
        "<tool_call>",
        "<toolcall>",
        "<tool-call>",
        "<tool>",
        "<invoke>",
    ];

    fn find_first_tag<'a>(haystack: &str, tags: &'a [&'a str]) -> Option<(usize, &'a str)> {
        tags.iter()
            .filter_map(|tag| haystack.find(tag).map(|idx| (idx, *tag)))
            .min_by_key(|(idx, _)| *idx)
    }

    fn matching_close_tag(open_tag: &str) -> Option<&'static str> {
        match open_tag {
            "<function_calls>" => Some("</function_calls>"),
            "<function_call>" => Some("</function_call>"),
            "<tool_call>" => Some("</tool_call>"),
            "<toolcall>" => Some("</toolcall>"),
            "<tool-call>" => Some("</tool-call>"),
            "<tool>" => Some("</tool>"),
            "<invoke>" => Some("</invoke>"),
            _ => None,
        }
    }

    fn extract_first_json_end(input: &str) -> Option<usize> {
        let trimmed = input.trim_start();
        let trim_offset = input.len().saturating_sub(trimmed.len());

        for (byte_idx, ch) in trimmed.char_indices() {
            if ch != '{' && ch != '[' {
                continue;
            }

            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(_value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    return Some(trim_offset + byte_idx + consumed);
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

    fn tool_structure_runs_to_end(inner: &str) -> bool {
        let mut rest = inner.trim_start();
        while rest.starts_with('<') {
            match rest.find('>') {
                Some(gt) => rest = rest[gt + 1..].trim_start(),
                None => return true,
            }
        }
        let tail = rest.trim();
        if tail.is_empty() {
            return true;
        }
        !looks_like_prose(tail)
    }

    // Heuristic: does `text` read like resumed natural-language prose (as opposed
    // to a cut-off parameter value)? True on an internal sentence boundary
    // (". " / "! " / "? " + a letter) or a multi-word string that ends like a
    // sentence. Deliberately lenient so ambiguous tails are kept, not dropped.
    fn looks_like_prose(text: &str) -> bool {
        let bytes = text.as_bytes();
        for i in 0..bytes.len().saturating_sub(1) {
            if matches!(bytes[i], b'.' | b'!' | b'?')
                && matches!(bytes[i + 1], b' ' | b'\n' | b'\t')
                && text[i + 1..]
                    .trim_start()
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic())
            {
                return true;
            }
        }
        let trimmed = text.trim_end();
        let ends_like_sentence = trimmed
            .chars()
            .last()
            .is_some_and(|c| matches!(c, '.' | '!' | '?'))
            && trimmed
                .chars()
                .rev()
                .nth(1)
                .is_some_and(|c| c.is_alphabetic());
        ends_like_sentence && text.trim().contains(' ')
    }

    let mut kept_segments = Vec::new();
    let mut remaining = message;

    while let Some((start, open_tag)) = find_first_tag(remaining, &TOOL_CALL_OPEN_TAGS) {
        let before = &remaining[..start];
        if !before.is_empty() {
            kept_segments.push(before.to_string());
        }

        let Some(close_tag) = matching_close_tag(open_tag) else {
            break;
        };
        let after_open = &remaining[start + open_tag.len()..];

        if let Some(close_idx) = after_open.find(close_tag) {
            remaining = &after_open[close_idx + close_tag.len()..];
            continue;
        }

        if let Some(consumed_end) = extract_first_json_end(after_open) {
            remaining = strip_leading_close_tags(&after_open[consumed_end..]);
            continue;
        }

        let inner = after_open.trim_start();
        let inner_lower = inner.to_ascii_lowercase();
        let looks_like_tool_structure = inner_lower.starts_with("<invoke")
            || inner_lower.starts_with("<parameter")
            || inner_lower.starts_with("<tool")
            || inner_lower.starts_with("<function")
            || inner.starts_with('{')
            || inner.starts_with('[');
        if looks_like_tool_structure && tool_structure_runs_to_end(inner) {
            remaining = "";
            break;
        }

        kept_segments.push(remaining[start..].to_string());
        remaining = "";
        break;
    }

    if !remaining.is_empty() {
        kept_segments.push(remaining.to_string());
    }

    let mut result = kept_segments.concat();

    // Clean up any resulting blank lines (but preserve paragraphs)
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    result.trim().to_string()
}

/// Remove `<tool_result …>…</tool_result>` blocks (and a leading `[Tool results]`
/// header, if present) from a conversation-history entry so that stale tool
/// output is never presented to the LLM without the corresponding `<tool_call>`.
pub(crate) fn strip_tool_result_content(text: &str) -> String {
    static TOOL_RESULT_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?s)<tool_result[^>]*>.*?</tool_result>")
            .expect("TOOL_RESULT_RE regex must compile")
    });

    let cleaned = TOOL_RESULT_RE.replace_all(text, "");
    let cleaned = cleaned.trim();

    // If the only remaining content is the header, drop it entirely.
    if cleaned == "[Tool results]" || cleaned.is_empty() {
        return String::new();
    }

    cleaned.to_string()
}

pub(crate) fn strip_tool_summary_prefix(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("[Used tools:") {
        // Find the closing bracket, then skip it and any leading newline(s).
        if let Some(bracket_end) = rest.find(']') {
            let after_bracket = &rest[bracket_end + 1..];
            let trimmed = after_bracket.trim_start_matches('\n');
            if trimmed.is_empty() {
                return String::new();
            }
            return trimmed.to_string();
        }
    }
    text.to_string()
}
/// Strip `<think>...</think>` blocks from streaming draft text so reasoning
/// tokens are never shown to the user in partial updates.
pub(crate) fn strip_think_tags_inline(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        if let Some(start) = rest.find("<think>") {
            result.push_str(&rest[..start]);
            if let Some(end) = rest[start..].find("</think>") {
                rest = &rest[start + end + "</think>".len()..];
            } else {
                // Unclosed tag: drop the tail to avoid leaking partial reasoning.
                break;
            }
        } else {
            result.push_str(rest);
            break;
        }
    }
    result.trim().to_string()
}

/// Drop the tail from the first scratchpad envelope that has opened but not
/// closed.
///
/// Streaming needs this and final delivery does not: the final response is a
/// complete turn, whereas a draft is rendered from whatever tokens have
/// arrived, and the closing tag can be hundreds of tokens away. Showing the
/// open envelope in the meantime is precisely the leak this guards against.
/// Nothing is lost — the next delta re-renders from the full accumulation.
///
/// A *closed* block is stepped over rather than cut at. On the paths that
/// strip closed blocks first there is nothing left to step over, so this costs
/// nothing there; it is what makes the function safe to run on the branch that
/// deliberately preserves a complete `<tool_call>` example, where cutting at
/// the first opener would delete the very thing the branch exists to keep.
fn truncate_at_unclosed_scratchpad_open(s: &str) -> String {
    let mut cut = s.len();
    let mut from = 0;
    while let Some(offset) = s[from..].find('<') {
        let open_at = from + offset;
        let Some(name) = protocol_tag_name_at(&s[open_at..]) else {
            // Not a protocol opener: step past this `<` and keep scanning, so
            // ordinary markup or prose does not end the search early.
            from = open_at + 1;
            continue;
        };
        let closer = format!("</{name}>");
        match s[open_at..].find(&closer) {
            // Complete block: resume after it, so a later opener is still
            // evaluated on its own merits.
            Some(close_at) => from = open_at + close_at + closer.len(),
            // Nothing closes this one, so it is still mid-emission.
            None => {
                cut = open_at;
                break;
            }
        }
    }

    let head = &s[..cut];
    // The opening tag is itself delivered in fragments, so the tail can be a
    // strict prefix of an opener ("…\n<tool_res") that matches no complete name
    // yet. Cutting only on the complete literal renders that fragment to the
    // user for one frame — the leak this function exists to prevent. The
    // fragment is restored by the next delta if it turns out to be prose.
    let partial = head
        .rfind('<')
        .filter(|&pos| is_partial_protocol_tag_open(&head[pos..]))
        .unwrap_or(head.len());
    head[..partial].trim_end().to_string()
}

/// The protocol element name opening at the start of `rest`, if any.
///
/// Longest match wins, so `<tool_call>` is never read as the shorter `<tool>`
/// and mistakenly hunted for a `</tool>` that will never arrive. The name must
/// be followed by a character that actually terminates an element name, so
/// prose like `<toolkit>` is not mistaken for protocol.
fn protocol_tag_name_at(rest: &str) -> Option<&'static str> {
    tool_protocol_tag_names().find(|name| {
        let Some(after) = rest.get(1..1 + name.len()) else {
            return false;
        };
        if !after.eq_ignore_ascii_case(name) {
            return false;
        }
        rest[1 + name.len()..]
            .chars()
            .next()
            .is_some_and(|c| c == '>' || c == '/' || c.is_whitespace())
    })
}

/// Whether `rest` is a still-incomplete opener — a strict prefix of some
/// protocol element name, with nothing after it yet to say otherwise.
fn is_partial_protocol_tag_open(rest: &str) -> bool {
    let Some(typed) = rest.strip_prefix('<') else {
        return false;
    };
    tool_protocol_tag_names().any(|name| {
        name.len() >= typed.len()
            && name
                .get(..typed.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(typed))
    })
}

/// Sanitize a streaming draft partial before it is shown to the user.
///
/// Draft text is model output mid-flight, so it can carry scratchpad that the
/// delivered response never keeps: reasoning traces, and — because native
/// tool-call providers interleave narration with protocol — raw
/// `<tool_call>` / `<tool_result>` envelopes. Final replies are cleaned by
/// [`sanitize_channel_response_for_format_with_leak_detection`], but drafts
/// never reach it: `update_draft` posts straight to the channel transport, so
/// a leaked envelope stays on screen until the final edit replaces it, and
/// remains visible indefinitely if the turn fails first.
///
/// This is the assistant-output boundary for partial text, and it keeps the
/// final sanitizer's preservation contract rather than a looser one: an
/// answer that *is* documentation for `<tool_call>` keeps its tags here
/// exactly as it keeps them through final delivery, while `<tool_result>`
/// envelopes and reasoning go in both places. Placing the filter here rather
/// than in a channel transport is deliberate — transports also carry
/// attachment captions, announcements, and operator text, none of which are
/// assistant scratchpad.
pub(crate) fn sanitize_streaming_draft_text(s: &str, known_tool_names: &HashSet<String>) -> String {
    let cleaned = strip_think_tags_inline(s);

    // Same classifier the delivered message is judged by, for the same reason:
    // XML tags are only one of the shapes protocol arrives in. A provider with
    // `strict_tool_parsing` enabled forwards deltas without passing them
    // through the runtime's `StreamTextGuard`, so a bare or fenced protocol
    // JSON body reaches this boundary exactly as the model emitted it. The
    // classifier is partial-aware where it matters — an envelope whose JSON has
    // not finished arriving fails to parse and is caught as malformed — and it
    // exempts genuine protocol documentation, so an answer *about* tool calls
    // is not blanked.
    //
    // Blanking a frame is the safe direction: an accumulation that momentarily
    // classifies as protocol and later resolves to prose is re-rendered whole
    // by the next delta, whereas a leaked envelope stays on screen until the
    // final edit, or forever if the turn fails first.
    if should_suppress_top_level_tool_protocol_response(cleaned.trim(), known_tool_names) {
        return String::new();
    }

    // Mirror the final sanitizer's guards exactly rather than inventing a
    // looser draft contract: there, the tool-CALL pass is skipped for genuine
    // protocol examples while the tool-RESULT pass runs unconditionally.
    // Preserving more here than final delivery preserves would put content on
    // screen that the delivered message then strips — a leak window in the
    // one direction this fix exists to close.
    let cleaned = if starts_with_visible_tool_call_tag_example(&cleaned) {
        // Preserving the example does not license showing a half-emitted
        // result envelope: `strip_tool_result_content` removes the closed
        // ones, and truncation removes an opener that has no closer yet.
        // Skipping truncation here would leave a raw partial payload on
        // screen, which is the same leak this function exists to close.
        strip_tool_result_content(&cleaned)
    } else {
        let cleaned = strip_tool_call_tags(&cleaned);
        strip_tool_result_content(&cleaned)
    };

    // Embedded protocol, as opposed to a whole-response envelope: narration
    // followed by a fenced or bare JSON payload. Both passes only act on
    // complete blocks, which is why the truncations below still have work to do.
    let cleaned = strip_fenced_tool_protocol_artifacts(&cleaned, known_tool_names);
    let cleaned = strip_isolated_tool_json_artifacts(&cleaned, known_tool_names);

    let cleaned = truncate_at_unclosed_protocol_fence(&cleaned, known_tool_names);
    let cleaned = truncate_at_incomplete_protocol_json(&cleaned);
    truncate_at_unclosed_scratchpad_open(&cleaned)
}

/// Drop the tail from a JSON value that has started, already reads as tool
/// protocol, and has not finished arriving.
///
/// This is the JSON counterpart to [`truncate_at_unclosed_scratchpad_open`],
/// and it exists for the same reason: the completed-payload passes cannot
/// classify a value they cannot parse, so without it the first frames of a
/// protocol envelope render verbatim — `{"tool_call_id":"call_1",` on screen
/// while the rest is still coming.
///
/// Complete values are stepped over rather than cut at, so narration followed
/// by finished JSON is judged by the completed-payload passes as before. Only
/// the unfinished tail is held back, and only when the parser recognizes it as
/// protocol, so an ordinary JSON answer still streams as it arrives.
fn truncate_at_incomplete_protocol_json(s: &str) -> String {
    let mut from = 0;
    while let Some(rel) = s[from..].find(['{', '[']) {
        let at = from + rel;
        let tail = &s[at..];
        let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<serde_json::Value>();
        match stream.next() {
            Some(Ok(_)) if stream.byte_offset() > 0 => from = at + stream.byte_offset(),
            // Nothing completes from here, so this is the unfinished tail and
            // everything after it belongs to the same value.
            _ => {
                return if zeroclaw_tool_call_parser::looks_like_incomplete_tool_protocol_json(tail)
                {
                    s[..at].trim_end().to_string()
                } else {
                    s.to_string()
                };
            }
        }
    }
    s.to_string()
}

/// Drop the tail from a fenced block that has opened, already reads as tool
/// protocol, and has not closed yet.
///
/// The completed-block passes cannot help here: a fence is only recognized once
/// its closing ``` arrives, which for a protocol payload can be the rest of the
/// turn. Waiting renders the payload meanwhile.
///
/// The cut is conditional on the partial body *already* classifying as
/// protocol, so an ordinary fenced code block still streams line by line as the
/// user expects; only a block that has shown its protocol shape is held back.
fn truncate_at_unclosed_protocol_fence(s: &str, known_tool_names: &HashSet<String>) -> String {
    let mut cursor = 0usize;
    while let Some(rel_open) = s[cursor..].find("```") {
        let open_start = cursor + rel_open;
        let after_ticks = open_start + 3;
        let Some(line_end_rel) = s[after_ticks..].find('\n') else {
            // The language tag itself is still arriving; nothing to judge yet.
            return s.to_string();
        };
        let body_start = after_ticks + line_end_rel + 1;
        match s[body_start..].find("```") {
            Some(close_rel) => cursor = body_start + close_rel + 3,
            None => {
                let body = s[body_start..].trim();
                return if should_suppress_top_level_tool_protocol_response(body, known_tool_names) {
                    s[..open_start].trim_end().to_string()
                } else {
                    s.to_string()
                };
            }
        }
    }
    s.to_string()
}

fn starts_with_visible_tool_call_tag_example(response: &str) -> bool {
    let lower = response.trim_start().to_ascii_lowercase();
    let starts_with_tool_tag = lower.starts_with("<tool_call")
        || lower.starts_with("<toolcall")
        || lower.starts_with("<tool-call")
        || lower.starts_with("<invoke");

    starts_with_tool_tag && zeroclaw_tool_call_parser::looks_like_tool_protocol_example(response)
}

fn should_suppress_top_level_tool_protocol_response(
    response: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    if zeroclaw_tool_call_parser::looks_like_tool_protocol_example(response) {
        return false;
    }

    if zeroclaw_tool_call_parser::looks_like_malformed_tool_protocol_envelope_for_known_tools(
        response,
        known_tool_names,
    ) {
        return true;
    }

    if let Some(kind) = zeroclaw_tool_call_parser::classify_tool_protocol_envelope(response) {
        return matches!(
            kind,
            zeroclaw_tool_call_parser::ToolProtocolEnvelopeKind::TaggedToolCall
        ) || (!known_tool_names.is_empty()
            && (matches!(
                kind,
                zeroclaw_tool_call_parser::ToolProtocolEnvelopeKind::ToolResult
            ) || zeroclaw_tool_call_parser::tool_protocol_envelope_mentions_known_tool(
                response,
                known_tool_names,
            )));
    }

    // If the broad envelope detector still matches after classification failed,
    // this is malformed internal protocol JSON rather than ordinary content.
    zeroclaw_tool_call_parser::looks_like_tool_protocol_envelope(response)
}

#[cfg(test)]
pub(crate) fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    sanitize_channel_response_with_leak_detection(
        response,
        tools,
        &zeroclaw_config::schema::LeakDetectionConfig::default(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutboundContentFormat {
    Markdown,
    PlainText,
}

pub(crate) fn outbound_content_format_for_channel(channel: &str) -> OutboundContentFormat {
    let channel_type = channel
        .split_once('.')
        .map_or(channel, |(channel_type, _)| channel_type);
    if channel_type.eq_ignore_ascii_case("irc") || channel_type.eq_ignore_ascii_case("twitch") {
        OutboundContentFormat::PlainText
    } else {
        OutboundContentFormat::Markdown
    }
}

#[cfg(test)]
pub(crate) fn sanitize_channel_response_with_leak_detection(
    response: &str,
    tools: &[Box<dyn Tool>],
    leak_detection: &zeroclaw_config::schema::LeakDetectionConfig,
) -> String {
    sanitize_channel_response_for_format_with_leak_detection(
        response,
        tools,
        leak_detection,
        OutboundContentFormat::Markdown,
    )
}

pub(crate) fn sanitize_channel_response_for_format_with_leak_detection(
    response: &str,
    tools: &[Box<dyn Tool>],
    leak_detection: &zeroclaw_config::schema::LeakDetectionConfig,
    content_format: OutboundContentFormat,
) -> String {
    let known_tool_names: HashSet<String> = tools
        .iter()
        .map(|tool| tool.name().to_ascii_lowercase())
        .collect();
    // Strip any [Used tools: ...] prefix that the LLM may have echoed from
    // history context. Trim first to handle leading/trailing whitespace.
    let trimmed_response = response.trim();
    let trimmed_response = strip_think_tags_inline(trimmed_response).trim().to_string();
    let trimmed_response = trimmed_response.as_str();
    // Final channel guardrail: reuse the parser classifier so channel cleanup
    // cannot drift from runtime tool-protocol detection.
    if should_suppress_top_level_tool_protocol_response(trimmed_response, &known_tool_names) {
        return String::new();
    }
    let stripped_summary = strip_tool_summary_prefix(trimmed_response);
    let stripped_xml = if starts_with_visible_tool_call_tag_example(&stripped_summary) {
        stripped_summary
    } else {
        strip_tool_call_tags(&stripped_summary)
    };
    let stripped_results = strip_tool_result_content(&stripped_xml);
    let stripped_fenced_json =
        strip_fenced_tool_protocol_artifacts(&stripped_results, &known_tool_names);
    let stripped_json =
        strip_isolated_tool_json_artifacts(&stripped_fenced_json, &known_tool_names);
    // Strip leading narration lines that announce tool usage
    let sanitized = strip_tool_narration(&stripped_json);

    redact_channel_outbound_leaks(&sanitized, leak_detection, content_format)
}

pub(crate) fn redact_channel_outbound_leaks(
    content: &str,
    leak_detection: &zeroclaw_config::schema::LeakDetectionConfig,
    content_format: OutboundContentFormat,
) -> String {
    if !leak_detection.enabled {
        return content.to_string();
    }
    // Scan for credential leaks before returning to caller. Format-specific
    // outbound layers identify parsed destinations that must remain intact and
    // pass only byte ranges to the format-agnostic detector.
    let protected_spans = channel_outbound_protected_spans(content, content_format);
    match zeroclaw_runtime::security::LeakDetector::with_config(leak_detection)
        .scan_with_protected_spans(content, &protected_spans)
    {
        zeroclaw_runtime::security::LeakResult::Clean => content.to_string(),
        zeroclaw_runtime::security::LeakResult::Detected { patterns, redacted } => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"patterns": patterns})),
                "output guardrail: credential leak detected in outbound channel response"
            );
            redacted
        }
    }
}

pub(crate) fn channel_outbound_protected_spans(
    content: &str,
    content_format: OutboundContentFormat,
) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    // A file URI is a file reference even when punctuation inside it looks
    // like query syntax; protect it for every outbound text format.
    if content
        .as_bytes()
        .windows(b"file:".len())
        .any(|window| window.eq_ignore_ascii_case(b"file:"))
    {
        collect_raw_file_uri_spans(content, &mut spans);
    }
    match content_format {
        OutboundContentFormat::Markdown => {
            if content.contains("](")
                || content.contains("]:")
                || (content.contains('<') && content.contains("://"))
            {
                collect_markdown_link_destination_spans(content, &mut spans);
            }
        }
        OutboundContentFormat::PlainText => {}
    }
    spans
}

fn collect_markdown_link_destination_spans(content: &str, spans: &mut Vec<Range<usize>>) {
    let parser = MarkdownParser::new_ext(content, MarkdownOptions::empty());

    for (_, link_def) in parser.reference_definitions().iter() {
        if let Some(span) =
            parsed_destination_span(content, link_def.span.clone(), link_def.dest.as_ref())
        {
            spans.push(span);
        }
    }

    for (event, range) in parser.into_offset_iter() {
        if let Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) = event
            && let Some(span) = parsed_destination_span(content, range, dest_url.as_ref())
        {
            spans.push(span);
        }
    }
}

fn parsed_destination_span(
    content: &str,
    source_range: Range<usize>,
    parsed_destination: &str,
) -> Option<Range<usize>> {
    if parsed_destination.is_empty() {
        return None;
    }
    let source = content.get(source_range.clone())?;
    let search_start = destination_search_start(source);
    decoded_destination_span(source, search_start, parsed_destination)
        .map(|span| source_range.start + span.start..source_range.start + span.end)
}

fn destination_search_start(source: &str) -> usize {
    source
        .find("](")
        .map(|idx| idx + 2)
        .or_else(|| source.find("]:").map(|idx| idx + 2))
        .unwrap_or(0)
}

fn decoded_destination_span(
    source: &str,
    search_start: usize,
    parsed_destination: &str,
) -> Option<Range<usize>> {
    for (offset, _) in source[search_start..].char_indices() {
        let start = search_start + offset;
        if let Some(end) = decoded_match_end(&source[start..], parsed_destination) {
            return Some(start..start + end);
        }
    }
    None
}

fn decoded_match_end(raw: &str, parsed: &str) -> Option<usize> {
    let mut raw_idx = 0;

    for expected in parsed.chars() {
        let ch = raw[raw_idx..].chars().next()?;
        let (decoded, end) = if ch == '\\' {
            let escaped_idx = raw_idx + ch.len_utf8();
            let next_ch = raw[escaped_idx..].chars().next()?;
            if !next_ch.is_ascii_punctuation() {
                return None;
            }
            (next_ch, escaped_idx + next_ch.len_utf8())
        } else if ch == '&' {
            decode_markdown_entity(raw, raw_idx)?
        } else {
            (ch, raw_idx + ch.len_utf8())
        };

        if decoded != expected {
            return None;
        }
        raw_idx = end;
    }

    Some(raw_idx)
}

fn decode_markdown_entity(raw: &str, amp_idx: usize) -> Option<(char, usize)> {
    let entity_end = raw[amp_idx..].find(';')? + amp_idx + 1;
    let entity = &raw[amp_idx + 1..entity_end - 1];
    let decoded = match entity {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" | "APOS" => '\'',
        "colon" | "COLON" => ':',
        "sol" | "SOL" => '/',
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            let value = u32::from_str_radix(&entity[2..], 16).ok()?;
            char::from_u32(value)?
        }
        _ if entity.starts_with('#') => {
            let value = entity[1..].parse::<u32>().ok()?;
            char::from_u32(value)?
        }
        _ => return None,
    };
    Some((decoded, entity_end))
}

fn collect_raw_file_uri_spans(content: &str, spans: &mut Vec<Range<usize>>) {
    let mut token_start = None;

    for (idx, ch) in content.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = token_start.take() {
                collect_file_uri_token_span(content, start, idx, spans);
            }
        } else {
            token_start.get_or_insert(idx);
        }
    }

    if let Some(start) = token_start {
        collect_file_uri_token_span(content, start, content.len(), spans);
    }
}

fn collect_file_uri_token_span(
    content: &str,
    token_start: usize,
    token_end: usize,
    spans: &mut Vec<Range<usize>>,
) {
    let token = &content[token_start..token_end];
    let trimmed_start = token
        .char_indices()
        .find(|(_, ch)| !matches!(ch, '<' | '(' | '[' | '{' | '"' | '\''))
        .map_or(token.len(), |(idx, _)| idx);
    let trimmed_end = token
        .char_indices()
        .rev()
        .find(|(_, ch)| {
            !matches!(
                ch,
                '>' | ')' | ']' | '}' | '"' | '\'' | '.' | ',' | ';' | ':'
            )
        })
        .map_or(trimmed_start, |(idx, ch)| idx + ch.len_utf8());

    if trimmed_start >= trimmed_end {
        return;
    }

    let trimmed = &token[trimmed_start..trimmed_end];
    let Some(scheme_offset) = trimmed
        .as_bytes()
        .windows(b"file:".len())
        .position(|window| window.eq_ignore_ascii_case(b"file:"))
    else {
        return;
    };
    let uri_start = trimmed_start + scheme_offset;
    let candidate = &token[uri_start..trimmed_end];

    if Url::parse(candidate).is_ok_and(|url| url.scheme().eq_ignore_ascii_case("file")) {
        spans.push(token_start + uri_start..token_start + trimmed_end);
    }
}

/// Shown when the agent turn completes but no visible text remains after sanitization.
pub(crate) const EMPTY_CHANNEL_REPLY_FALLBACK: &str =
    "I couldn't produce a visible reply for that message. Please try again.";

/// Ensure channel outbound text is never empty so users don't see typing with no message.
pub(crate) fn ensure_nonempty_channel_reply(
    delivered_response: String,
    outbound_response: &str,
    channel: &str,
    reply_target: &str,
) -> String {
    if !delivered_response.trim().is_empty() {
        return delivered_response;
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "channel": channel,
                "reply_target": reply_target,
                "outbound_len": outbound_response.len(),
            })),
        "channel_reply_empty; substituting fallback"
    );
    EMPTY_CHANNEL_REPLY_FALLBACK.to_string()
}

/// Remove leading lines that narrate tool usage (e.g. "Let me check the weather for you.").
/// Only strips lines from the very beginning of the message that match common
/// narration patterns, so genuine content is preserved.
fn strip_tool_narration(message: &str) -> String {
    let narration_prefixes: &[&str] = &[
        "let me ",
        "i'll ",
        "i will ",
        "i am going to ",
        "i'm going to ",
        "searching ",
        "looking up ",
        "fetching ",
        "checking ",
        "using the ",
        "using my ",
        "one moment",
        "hold on",
        "just a moment",
        "give me a moment",
        "allow me to ",
    ];

    let mut result_lines: Vec<&str> = Vec::new();
    let mut past_narration = false;

    for line in message.lines() {
        if past_narration {
            result_lines.push(line);
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if narration_prefixes.iter().any(|p| lower.starts_with(p)) {
            // Skip this narration line
            continue;
        }
        // First non-narration, non-empty line — keep everything from here
        past_narration = true;
        result_lines.push(line);
    }

    let joined = result_lines.join("\n");
    let trimmed = joined.trim();
    if trimmed.is_empty() && !message.trim().is_empty() {
        // If stripping removed everything, return original to avoid empty reply
        message.to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_tool_call_payload(value: &serde_json::Value, known_tool_names: &HashSet<String>) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let (name, has_args) =
        if let Some(function) = object.get("function").and_then(|f| f.as_object()) {
            (
                function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| object.get("name").and_then(|v| v.as_str())),
                function.contains_key("arguments")
                    || function.contains_key("parameters")
                    || object.contains_key("arguments")
                    || object.contains_key("parameters"),
            )
        } else {
            (
                object.get("name").and_then(|v| v.as_str()),
                object.contains_key("arguments") || object.contains_key("parameters"),
            )
        };

    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return false;
    };

    has_args && known_tool_names.contains(&name.to_ascii_lowercase())
}

fn is_tool_result_payload(
    object: &serde_json::Map<String, serde_json::Value>,
    saw_tool_call_payload: bool,
) -> bool {
    if !saw_tool_call_payload || !object.contains_key("result") {
        return false;
    }

    object.keys().all(|key| {
        matches!(
            key.as_str(),
            "result" | "id" | "tool_call_id" | "name" | "tool"
        )
    })
}

fn sanitize_tool_json_value(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
    saw_tool_call_payload: bool,
) -> Option<(String, bool)> {
    if let Some(kind) =
        zeroclaw_tool_call_parser::classify_tool_protocol_envelope(&value.to_string())
    {
        if known_tool_names.is_empty() {
            return None;
        }

        if matches!(
            kind,
            zeroclaw_tool_call_parser::ToolProtocolEnvelopeKind::ToolResult
        ) {
            return Some((String::new(), true));
        }

        if !zeroclaw_tool_call_parser::tool_protocol_envelope_mentions_known_tool(
            &value.to_string(),
            known_tool_names,
        ) {
            return None;
        }

        let content = safe_protocol_envelope_content(value);
        return Some((content, true));
    }

    if is_tool_call_payload(value, known_tool_names) {
        return Some((String::new(), true));
    }

    if let Some(array) = value.as_array() {
        if !array.is_empty()
            && array
                .iter()
                .all(|item| is_tool_call_payload(item, known_tool_names))
        {
            return Some((String::new(), true));
        }
        return None;
    }

    let object = value.as_object()?;

    if let Some(tool_calls) = object.get("tool_calls").and_then(|value| value.as_array())
        && !tool_calls.is_empty()
        && tool_calls
            .iter()
            .all(|call| is_tool_call_payload(call, known_tool_names))
    {
        let content = object
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        return Some((content, true));
    }

    if is_tool_result_payload(object, saw_tool_call_payload) {
        return Some((String::new(), false));
    }

    None
}

fn safe_protocol_envelope_content(value: &serde_json::Value) -> String {
    let content = value
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();

    if content.is_empty()
        || zeroclaw_tool_call_parser::looks_like_tool_protocol_envelope(content)
        || zeroclaw_tool_call_parser::looks_like_malformed_tool_protocol_envelope(content)
    {
        return String::new();
    }

    content.to_string()
}

fn is_line_isolated_json_segment(message: &str, start: usize, end: usize) -> bool {
    let line_start = message[..start].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = message[end..]
        .find('\n')
        .map_or(message.len(), |idx| end + idx);

    message[line_start..start].trim().is_empty() && message[end..line_end].trim().is_empty()
}

fn is_inside_markdown_code_fence(message: &str, index: usize) -> bool {
    // This intentionally uses a lightweight fence parity check. The sanitizer only
    // needs to avoid re-processing JSON in ordinary triple-backtick fences that
    // `strip_fenced_tool_protocol_artifacts` already handles; it is not a full
    // Markdown parser for inline code spans or longer fence runs.
    let mut in_fence = false;
    let mut cursor = 0usize;
    while let Some(rel_pos) = message[cursor..index].find("```") {
        in_fence = !in_fence;
        cursor += rel_pos + 3;
    }
    in_fence
}

fn isolated_malformed_tool_protocol_segment_end(
    message: &str,
    start: usize,
    known_tool_names: &HashSet<String>,
) -> Option<usize> {
    let line_start = message[..start].rfind('\n').map_or(0, |idx| idx + 1);
    if !message[line_start..start].trim().is_empty() {
        return None;
    }

    let mut end = start;
    // Malformed JSON has no serde byte offset. Scan forward from an isolated
    // JSON candidate start, but stop before ordinary prose resumes.
    for line in message[start..].split_inclusive('\n') {
        let trimmed = line.trim();
        if end > start
            && !trimmed.is_empty()
            && !trimmed.starts_with(['{', '[', ']', '}'])
            && !trimmed.starts_with('"')
        {
            break;
        }
        end += line.len();
        let candidate = &message[start..end];
        if zeroclaw_tool_call_parser::looks_like_malformed_tool_protocol_envelope_for_known_tools(
            candidate,
            known_tool_names,
        ) {
            return Some(end);
        }
    }

    None
}

fn is_tool_protocol_fence_language(language: &str) -> bool {
    let lower = language.trim().to_ascii_lowercase();
    lower == "tool_call"
        || lower == "toolcall"
        || lower == "tool-call"
        || lower == "invoke"
        || lower
            .strip_prefix("tool")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace) && !rest.trim().is_empty())
}

fn strip_fenced_tool_protocol_artifacts(
    message: &str,
    known_tool_names: &HashSet<String>,
) -> String {
    if zeroclaw_tool_call_parser::looks_like_tool_protocol_example(message) {
        return message.to_string();
    }

    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;

    while let Some(rel_open) = message[cursor..].find("```") {
        let open_start = cursor + rel_open;
        let language_start = open_start + 3;
        let Some(line_end_rel) = message[language_start..].find('\n') else {
            break;
        };
        let line_end = language_start + line_end_rel;
        let language = message[language_start..line_end]
            .trim()
            .trim_end_matches('\r');
        let body_start = line_end + 1;
        let Some(close_rel) = message[body_start..].find("```") else {
            break;
        };
        let close_start = body_start + close_rel;
        let close_end = close_start + 3;

        let fence_block = &message[open_start..close_end];
        let should_strip = if language.eq_ignore_ascii_case("json") {
            should_suppress_top_level_tool_protocol_response(
                message[body_start..close_start].trim(),
                known_tool_names,
            )
        } else {
            is_tool_protocol_fence_language(language)
                && zeroclaw_tool_call_parser::contains_tool_protocol_tag_call(fence_block)
        };

        if should_strip {
            cleaned.push_str(&message[cursor..open_start]);
            cursor = close_end;
            continue;
        }

        cleaned.push_str(&message[cursor..close_end]);
        cursor = close_end;
    }

    cleaned.push_str(&message[cursor..]);
    cleaned
}

pub(crate) fn strip_isolated_tool_json_artifacts(
    message: &str,
    known_tool_names: &HashSet<String>,
) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;
    let mut saw_tool_call_payload = false;

    while cursor < message.len() {
        let Some(rel_start) = message[cursor..].find(['{', '[']) else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + rel_start;
        cleaned.push_str(&message[cursor..start]);
        if is_inside_markdown_code_fence(message, start) {
            let Some(ch) = message[start..].chars().next() else {
                break;
            };
            cleaned.push(ch);
            cursor = start + ch.len_utf8();
            continue;
        }

        let candidate = &message[start..];
        let mut stream =
            serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();

        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                let end = start + consumed;
                if is_line_isolated_json_segment(message, start, end)
                    && let Some((replacement, marks_tool_call)) =
                        sanitize_tool_json_value(&value, known_tool_names, saw_tool_call_payload)
                {
                    if marks_tool_call {
                        saw_tool_call_payload = true;
                    }
                    if !replacement.trim().is_empty() {
                        cleaned.push_str(replacement.trim());
                    }
                    cursor = end;
                    continue;
                }
            }
        }

        if let Some(end) =
            isolated_malformed_tool_protocol_segment_end(message, start, known_tool_names)
        {
            cursor = end;
            continue;
        }

        let Some(ch) = message[start..].chars().next() else {
            break;
        };
        cleaned.push(ch);
        cursor = start + ch.len_utf8();
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

#[cfg(test)]
mod streaming_draft_tests {
    use super::*;
    use std::collections::HashSet;

    fn no_tools() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn streaming_draft_strips_tool_result_envelopes() {
        assert_eq!(
            sanitize_streaming_draft_text(
                "Before.\n<tool_result name=\"shell\">{\"ok\":true}</tool_result>\nAfter.",
                &no_tools()
            ),
            "Before.\n\nAfter."
        );
        assert_eq!(
            sanitize_streaming_draft_text(
                "Result summary:\n<tool_result>\n{\"status\": \"ok\",",
                &no_tools()
            ),
            "Result summary:"
        );
    }

    #[test]
    fn streaming_draft_preserves_tool_protocol_example() {
        let example = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call>\nThis is an example, not an invocation.";
        assert_eq!(sanitize_streaming_draft_text(example, &no_tools()), example);
    }

    #[test]
    fn streaming_draft_leaves_clean_text_untouched() {
        let text = "A normal reply.\nWith two lines.\n\n  indented continuation";
        assert_eq!(sanitize_streaming_draft_text(text, &no_tools()), text);
        let prose = "Use the `tool_result` field.";
        assert_eq!(sanitize_streaming_draft_text(prose, &no_tools()), prose);
    }

    #[test]
    fn streaming_draft_does_not_treat_thinking_tag_as_scratchpad() {
        let text = "<thinking-cap>worn</thinking-cap> Answer.";
        assert_eq!(sanitize_streaming_draft_text(text, &no_tools()), text);
    }

    /// Parity guard: the draft path must not preserve MORE than final
    /// delivery does. `strip_tool_result_content` is unguarded in the shared
    /// sanitizer, so a `<tool_result>` envelope goes even inside an otherwise
    /// preserved protocol example — otherwise the draft would display content
    /// the delivered message strips moments later.
    #[test]
    fn streaming_draft_strips_tool_result_even_inside_a_preserved_example() {
        let text = "<tool_call>{\"name\":\"shell\"}</tool_call>\nThis is an example, not an invocation.\n<tool_result>{\"secret\":1}</tool_result>";
        let draft = sanitize_streaming_draft_text(text, &no_tools());
        assert!(
            !draft.contains("tool_result") && !draft.contains("secret"),
            "tool_result must be stripped even in an example: {draft:?}"
        );
        assert!(
            draft.contains("This is an example, not an invocation."),
            "the example prose must survive: {draft:?}"
        );

        // Same input through the shared final sanitizer: the two boundaries
        // must agree on what survives.
        let final_text = sanitize_channel_response(text, &[]);
        assert_eq!(
            draft.contains("tool_result"),
            final_text.contains("tool_result"),
            "draft and final must agree on tool_result handling"
        );
    }

    /// Composed regression over the streaming loop rather than a single
    /// string: replay the accumulation `draft_updater` performs and assert no
    /// intermediate frame ever renders a scratchpad envelope. A per-call test
    /// alone would miss a leak that is only visible mid-stream, which is
    /// exactly how the reported artifact reached the user.
    #[test]
    fn streaming_draft_never_renders_scratchpad_in_any_frame() {
        let deltas = [
            "Checking the price",
            " now.\n<tool_res",
            "ult name=\"price\">",
            "{\"btc\": 64000}",
            "</tool_result>\n",
            "BTC is $64,000.",
        ];
        let mut accumulated = String::new();
        let mut frames = Vec::new();
        for delta in deltas {
            accumulated.push_str(delta);
            frames.push(sanitize_streaming_draft_text(&accumulated, &no_tools()));
        }

        for (i, frame) in frames.iter().enumerate() {
            assert!(
                !frame.contains("<tool_res") && !frame.contains("btc"),
                "frame {i} leaked scratchpad: {frame:?}"
            );
        }
        assert_eq!(
            frames.last().unwrap(),
            "Checking the price now.\n\nBTC is $64,000."
        );
    }

    /// The preserved-example branch and a half-emitted result envelope in the
    /// same stream. Preserving a complete `<tool_call>` documentation block
    /// must not buy the model a window in which a partial `<tool_result>`
    /// payload is visible, so this replays the accumulation frame by frame
    /// rather than sanitizing the finished string: the leak exists only while
    /// the closing tag has not arrived, which a single-string test cannot see.
    #[test]
    fn streaming_draft_preserved_example_never_shows_a_partial_result_tail() {
        let deltas = [
            "<tool_call>{\"name\":\"shell\"}",
            "</tool_call>\nThis is an example, not an invocation.",
            "\n<tool_result>{\"secret\":1",
            "}</tool_result>\nDone.",
        ];
        let mut accumulated = String::new();
        let mut frames = Vec::new();
        for delta in deltas {
            accumulated.push_str(delta);
            frames.push(sanitize_streaming_draft_text(&accumulated, &no_tools()));
        }

        for (i, frame) in frames.iter().enumerate() {
            assert!(
                !frame.contains("secret") && !frame.contains("<tool_result"),
                "frame {i} leaked a result envelope: {frame:?}"
            );
        }

        // Once the example is complete it must stay visible, including across
        // the frames where the result envelope is still arriving.
        for (i, frame) in frames.iter().enumerate().skip(1) {
            assert!(
                frame.contains("This is an example, not an invocation."),
                "frame {i} dropped the preserved example: {frame:?}"
            );
            assert!(
                frame.contains("<tool_call>"),
                "frame {i} dropped the preserved tool_call tags: {frame:?}"
            );
        }

        assert!(
            frames.last().unwrap().contains("Done."),
            "the closing prose must survive: {:?}",
            frames.last().unwrap()
        );
    }
}
