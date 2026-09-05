use crate::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    ModelProvider, ProviderCapabilities, StreamChunk, StreamError, StreamEvent, StreamOptions,
    StreamResult, TokenUsage, ToolCall as ProviderToolCall,
};
use anyhow::Context;
use async_trait::async_trait;
use base64::Engine as _;
use futures_util::stream::{self, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use zeroclaw_api::tool::ToolSpec;

/// Anthropic's API documentation lists 1.0 as the default sampling temperature.
const TEMPERATURE_DEFAULT: f64 = 1.0;
/// Anthropic's public API endpoint. Overrideable via `model_providers.<name>.base_url`.
pub(crate) const BASE_URL: &str = "https://api.anthropic.com";
/// Anthropic's documented per-image ceiling for the direct API: 10 MB
/// **base64-encoded**. Measured on the encoded payload length, unlike the
/// multimodal config's `max_image_size_mb`, which bounds decoded bytes. MB is
/// read as 1024 * 1024 here, the same way `max_image_size_mb` reads it, so the
/// two ceilings stay consistent with each other. Anthropic's separate
/// per-request budget (32 MB across all images) is not enforced here.
const MAX_ENCODED_IMAGE_PAYLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Replaces a raw `data:<media type>;base64,<payload>` run that survived marker
/// parsing and would otherwise sit in a text position. See
/// [`AnthropicModelProvider::sweep_residual_image_data`].
///
/// Worded without "image" on purpose. The sweep matches any media type, because
/// any base64 blob in a text position is the token blowup it exists to stop, so
/// a note claiming an image was removed would be false for
/// `data:application/json;base64,…`. Like the omission note, this is prompt text
/// the model reads as fact.
const TRUNCATED_DATA_NOTE: &str = "[truncated inline data removed]";
/// Stand-in prose for a user message whose only content is an image, so the
/// message never ends on an `image` block — `apply_cache_to_last_message` is a
/// silent no-op on one, which would cost the request its cache breakpoint with
/// nothing reporting it. Used by the user arm of
/// [`AnthropicModelProvider::convert_messages`].
const IMAGE_ONLY_TEXT_PLACEHOLDER: &str = "[image]";
/// Stands in for a `tool_result` that never arrived, so an interrupted turn
/// cannot wedge the session with a hard 400 on replay. See
/// [`AnthropicModelProvider::backfill_orphaned_tool_uses`].
const INTERRUPTED_TOOL_RESULT_STUB: &str =
    "[tool result missing from history — the turn was interrupted before this tool finished]";
/// Stands in for a `tool_result` that did arrive but could not be attached to
/// this call, so it was omitted rather than handed to the model as
/// user-authored content. See
/// [`AnthropicModelProvider::backfill_orphaned_tool_uses`].
///
/// Opens with the same "tool result missing" phrase as
/// [`INTERRUPTED_TOOL_RESULT_STUB`] on purpose: the two are interchangeable
/// stubs, and a caller checking only that the result is missing should not have
/// to know which one it got.
const UNDELIVERED_TOOL_RESULT_STUB: &str = "[tool result missing from history — a result arrived \
     but could not be matched to this call, so it was not delivered]";
/// Prefix on tool output folded into the `tool_result` that already answered its
/// `tool_use`, because an earlier block in the same message got there first.
/// Without it the model reads the second answer as a continuation of the first.
///
/// The label sits **inside** the retained `tool_result`, not on a top-level
/// block. Tool output is untrusted, a top-level block in a user-role message
/// reads to the model as something the user typed, and naming the origin in
/// prose does not restore the structural boundary. See
/// [`AnthropicModelProvider::absorb_duplicate_tool_result`].
const DUPLICATE_TOOL_RESULT_PREFIX: &str = "[duplicate result for tool call";
/// Narrowest line width the residual sweep will read as line-wrapped base64. No
/// encoder wraps this narrow — MIME uses 76, PEM and `base64` use 64, Ruby uses
/// 60 — so below it a column of equal-length short tokens is far likelier than a
/// wrapped payload. See [`AnthropicModelProvider::residual_payload_end`].
const WRAPPED_BASE64_WIDTH_MIN: usize = 16;

use crate::stream_guard::AbortOnDrop;
use std::borrow::Cow;

/// Maximum silence between body reads for Anthropic SSE streams.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

pub struct AnthropicModelProvider {
    /// `[providers.models.anthropic.<alias>]` config-key alias.
    alias: String,
    credential: Option<String>,
    base_url: String,
    max_tokens: u32,
    timeout_secs: u64,
    /// Memoized cleaned tool schemas: each registered schema is cleaned once
    /// per provider instance (not once per request) and the byte-stable
    /// result keeps the `cache_control` tools block identical across
    /// requests. Note the memo only pays off while the instance lives —
    /// paths that rebuild the provider per call (e.g. the per-iteration
    /// vision route) start it empty each time.
    schema_cache: zeroclaw_api::schema::SchemaCleanCache,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ChatResponse {
    content: Vec<ContentBlock>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemPrompt>,
    messages: Vec<NativeMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<NativeThinkingConfig>,
}

#[derive(Debug, Serialize)]
struct NativeThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

fn anthropic_model_supports_native_thinking(model: &str) -> bool {
    !model.contains("claude-opus-4-7")
}

/// Characters legal between `data:` and `;base64,` in a data URI header: the
/// media type plus any parameters. Whitespace, commas and brackets are excluded
/// so a stray `data:` in prose cannot claim a `;base64,` further down the string.
fn is_data_uri_header_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '/' | '+' | '-' | '.' | '_' | ';' | '=')
}

/// The standard base64 alphabet plus its padding character.
fn is_base64_payload_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=')
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    content: Vec<NativeContentOut>,
}

#[derive(Debug, Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum NativeContentOut {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Thinking block for round-tripping extended thinking in conversation
    /// history. Required when thinking is enabled and assistant messages
    /// contain tool_use blocks.
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// `tool_result.content` accepts either a plain string or a list of nested
/// blocks. The string shape is **untagged**, so an image-free tool result still
/// serializes as a bare JSON string for `content` — byte-identical to what this
/// adapter sent before nested blocks existed.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

/// A block nested inside a `tool_result`. Anthropic also accepts `document` and
/// `search_result` here, but this adapter can only build `text` and `image`.
/// Keeping this separate from [`NativeContentOut`] makes a `tool_use` or a
/// nested `tool_result` — both of which the API rejects — unrepresentable.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ToolResultBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
}

/// The tool-result envelope this crate's runtime writes for a native tool call:
/// `{"tool_call_id": …, "content": "…"}`. Parsed, never serialized.
struct ToolResultEnvelope {
    /// `None` when `tool_call_id` is present but not a string — a shape the
    /// current turn engine does not emit but restored or externally supplied
    /// history can. The caller then tries to recover the id from the assistant
    /// turn this message follows.
    tool_use_id: Option<String>,
    /// The tool's own output, with the envelope scaffolding removed.
    content: String,
}

/// What the `"tool"` arm of [`AnthropicModelProvider::convert_messages`] knows
/// about the assistant turn it is currently answering.
///
/// `pending` and `answered` describe one run — the `tool_use` ids the most recent
/// assistant message emitted, and the subset a tool result has already answered.
/// Together they let a non-JSON tool carrier recover its `tool_use_id` when
/// exactly one candidate is left. Any message that ends the run clears both, so
/// recovery only ever pairs a result with the assistant turn it actually follows.
#[derive(Default)]
struct ToolResultRun {
    pending: Vec<String>,
    answered: std::collections::HashSet<String>,
    /// Calls whose result arrived but could not be attached to them, so it was
    /// dropped. Unlike the two above this spans the whole conversion and is never
    /// cleared: [`AnthropicModelProvider::backfill_orphaned_tool_uses`] reads it
    /// after the entire history is converted, to tell its stubs apart.
    ///
    /// Keyed by id rather than by message position because the backfill inserts
    /// messages as it walks the list, so any position captured earlier goes
    /// stale. One accepted imprecision, stated so nobody "fixes" it: a restored
    /// history that reuses a `tool_use_id` across assistant turns could put the
    /// dropped-result wording on the wrong turn's stub. Real histories do not
    /// reuse ids, and both wordings are stubs.
    undelivered: std::collections::HashSet<String>,
}

impl ToolResultRun {
    /// Starts a new run for the calls an assistant turn just made.
    fn begin(&mut self, pending: Vec<String>) {
        self.pending = pending;
        self.answered.clear();
    }

    /// Ends the current run, so no later tool message can pair with it.
    fn end(&mut self) {
        self.pending.clear();
        self.answered.clear();
    }

    fn mark_answered(&mut self, tool_use_id: &str) {
        self.answered.insert(tool_use_id.to_string());
    }

    /// The calls of this run that no tool result has answered yet.
    fn unanswered(&self) -> impl Iterator<Item = &str> + '_ {
        self.pending
            .iter()
            .map(String::as_str)
            .filter(|id| !self.answered.contains(*id))
    }

    /// The one call left unanswered, or `None` when there are none or several —
    /// the two cases where history does not prove the association and an id is
    /// never invented.
    fn only_unanswered(&self) -> Option<String> {
        let mut unanswered = self.unanswered();
        match (unanswered.next(), unanswered.next()) {
            (Some(only), None) => Some(only.to_string()),
            _ => None,
        }
    }

    /// Records every still-unanswered call of this run as one whose result was
    /// dropped, and returns how many there were.
    fn record_undelivered(&mut self) -> usize {
        let ids: Vec<String> = self.unanswered().map(str::to_string).collect();
        let count = ids.len();
        self.undelivered.extend(ids);
        count
    }

    /// The calls whose result arrived and was dropped.
    ///
    /// This is the only part of the run
    /// [`AnthropicModelProvider::backfill_orphaned_tool_uses`] is given. `pending`
    /// and `answered` describe the last assistant turn alone by the time it runs,
    /// so they would be wrong for every earlier turn it walks past.
    fn undelivered_ids(&self) -> &std::collections::HashSet<String> {
        &self.undelivered
    }
}

#[derive(Debug, Serialize)]
struct NativeToolSpec {
    name: String,
    description: String,
    /// `Arc`-shared with the tool registry's stored schema when no cleaning
    /// is required — serialized transparently, deep-cloned only for schemas
    /// the Anthropic cleaner actually rewrites
    input_schema: std::sync::Arc<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemPrompt {
    String(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    #[serde(default)]
    content: Vec<NativeContentIn>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    /// Tokens *after* the last cache breakpoint — NOT the total prompt.
    /// Per Anthropic prompt-caching docs:
    /// total_input = cache_read + cache_creation + input_tokens.
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Tokens served from the prompt cache this request.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    /// Tokens written to the prompt cache this request (cache miss path).
    /// Disjoint from `cache_read_input_tokens` and `input_tokens`.
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct NativeContentIn {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    /// Signature for integrity verification of thinking blocks.
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// Typed builder for [`AnthropicModelProvider`].
///
/// `alias` is the only positional argument. Everything else has a
/// sensible default: the base URL falls back to Anthropic's published
/// endpoint, no credential leaves the provider unauthenticated (fine
/// for local mocks), and token/timeout limits use the workspace baselines.
#[must_use]
pub struct AnthropicBuilder {
    alias: String,
    credential: Option<String>,
    base_url: Option<String>,
    max_tokens: Option<u32>,
    timeout_secs: Option<u64>,
}

impl AnthropicBuilder {
    /// Explicit API credential. Whitespace-only inputs are normalized
    /// to `None` so a stray `Some("   ")` from config cannot produce a
    /// bogus `Bearer    ` header.
    pub fn credential(mut self, credential: Option<&str>) -> Self {
        self.credential = credential
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(ToString::to_string);
        self
    }

    /// Override the API endpoint. Trailing slashes are stripped so
    /// callers need not care whether config supplied them.
    pub fn base_url(mut self, base_url: &str) -> Self {
        self.base_url = Some(base_url.trim_end_matches('/').to_string());
        self
    }

    /// Override the maximum output tokens for API requests. Defaults to
    /// [`zeroclaw_api::model_provider::BASELINE_MAX_TOKENS`] when unset.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Override the HTTP request timeout for LLM API calls. Defaults to
    /// [`zeroclaw_api::model_provider::BASELINE_TIMEOUT_SECS`] when unset.
    pub fn timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }

    pub fn build(self) -> AnthropicModelProvider {
        AnthropicModelProvider {
            alias: self.alias,
            credential: self.credential,
            base_url: self.base_url.unwrap_or_else(|| BASE_URL.to_string()),
            max_tokens: self
                .max_tokens
                .unwrap_or(zeroclaw_api::model_provider::BASELINE_MAX_TOKENS),
            timeout_secs: self
                .timeout_secs
                .unwrap_or(zeroclaw_api::model_provider::BASELINE_TIMEOUT_SECS),
            schema_cache: zeroclaw_api::schema::SchemaCleanCache::new(),
        }
    }
}

impl AnthropicModelProvider {
    /// Entry point. Only `alias` is required; every other field is set
    /// via a labelled chain method on the returned [`AnthropicBuilder`].
    pub fn builder(alias: &str) -> AnthropicBuilder {
        AnthropicBuilder {
            alias: alias.to_string(),
            credential: None,
            base_url: None,
            max_tokens: None,
            timeout_secs: None,
        }
    }

    fn is_setup_token(token: &str) -> bool {
        token.starts_with("sk-ant-oat01-")
    }

    fn apply_auth(
        &self,
        request: reqwest::RequestBuilder,
        credential: &str,
    ) -> reqwest::RequestBuilder {
        let is_setup = Self::is_setup_token(credential);
        let len = credential.len();
        let head: String = credential.chars().take(8).collect();
        let tail: String = credential
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"header": if is_setup { "Authorization" } else { "x-api-key" }, "credential_len": len, "credential_head": head, "credential_tail": tail})), "Anthropic auth header applied");
        if is_setup {
            request
                .header("Authorization", format!("Bearer {credential}"))
                .header(
                    "anthropic-beta",
                    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14",
                )
                .header("anthropic-dangerous-direct-browser-access", "true")
        } else {
            request.header("x-api-key", credential)
        }
    }

    /// For OAuth tokens, Anthropic requires the system prompt to start with the
    /// Claude Code identity prefix. This prepends it to any existing system prompt.
    fn apply_oauth_system_prompt(system: Option<SystemPrompt>) -> Option<SystemPrompt> {
        let prefix = SystemBlock {
            block_type: "text".to_string(),
            text: "You are Claude Code, Anthropic's official CLI for Claude.".to_string(),
            cache_control: Some(CacheControl::ephemeral()),
        };
        match system {
            Some(SystemPrompt::Blocks(mut blocks)) => {
                blocks.insert(0, prefix);
                Some(SystemPrompt::Blocks(blocks))
            }
            Some(SystemPrompt::String(s)) => Some(SystemPrompt::Blocks(vec![
                prefix,
                SystemBlock {
                    block_type: "text".to_string(),
                    text: s,
                    cache_control: Some(CacheControl::ephemeral()),
                },
            ])),
            None => Some(SystemPrompt::Blocks(vec![prefix])),
        }
    }

    /// Cache conversations with more than 1 non-system message (i.e. after first exchange)
    fn should_cache_conversation(messages: &[ChatMessage]) -> bool {
        messages.iter().filter(|m| m.role != "system").count() > 1
    }

    /// Apply cache control to the last message content block
    fn apply_cache_to_last_message(messages: &mut [NativeMessage]) {
        if let Some(last_msg) = messages.last_mut()
            && let Some(last_content) = last_msg.content.last_mut()
        {
            match last_content {
                NativeContentOut::Text { cache_control, .. }
                | NativeContentOut::ToolResult { cache_control, .. } => {
                    *cache_control = Some(CacheControl::ephemeral());
                }
                NativeContentOut::ToolUse { .. }
                | NativeContentOut::Image { .. }
                | NativeContentOut::Thinking { .. } => {}
            }
        }
    }

    fn convert_tools(&self, tools: Option<&[ToolSpec]>) -> Option<Vec<NativeToolSpec>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        let mut native_tools: Vec<NativeToolSpec> = items
            .iter()
            .map(|tool| NativeToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                // Cleaned at most once per registered schema per provider
                // instance (memoized), then `Arc`-shared into every request body.
                input_schema: self.schema_cache.clean_shared(
                    &tool.parameters,
                    zeroclaw_api::schema::CleaningStrategy::Anthropic,
                ),
                cache_control: None,
            })
            .collect();

        // Cache the last tool definition (caches all tools)
        if let Some(last_tool) = native_tools.last_mut() {
            last_tool.cache_control = Some(CacheControl::ephemeral());
        }

        Some(native_tools)
    }

    fn parse_assistant_tool_call_message(content: &str) -> Option<Vec<NativeContentOut>> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_calls = value
            .get("tool_calls")
            .and_then(|v| serde_json::from_value::<Vec<ProviderToolCall>>(v.clone()).ok())?;

        let mut blocks = Vec::new();

        // When extended thinking is enabled, assistant messages must start
        // with thinking blocks (including signatures) before any tool_use
        // blocks. The reasoning_content field stores JSON-encoded thinking
        // blocks from the original response.
        if let Some(reasoning) = value
            .get("reasoning_content")
            .and_then(serde_json::Value::as_str)
            .filter(|r| !r.is_empty())
        {
            for part in reasoning.split('\n') {
                if let Ok(block) = serde_json::from_str::<serde_json::Value>(part) {
                    let thinking = block
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let signature = block
                        .get("signature")
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    blocks.push(NativeContentOut::Thinking {
                        thinking,
                        signature,
                    });
                }
            }
        }

        if let Some(text) = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            blocks.push(NativeContentOut::Text {
                text: text.to_string(),
                cache_control: None,
            });
        }
        for call in tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            blocks.push(NativeContentOut::ToolUse {
                id: call.id,
                name: call.name,
                input,
                cache_control: None,
            });
        }
        Some(blocks)
    }

    /// Note appended to text when an image reference could not be sent.
    ///
    /// This is prompt text the model reads as fact, not user-facing UI text, so
    /// it stays an English literal rather than going through the Fluent
    /// catalogue.
    fn image_omission_note(count: usize) -> String {
        format!("[{count} image(s) omitted: unsupported or oversized image reference]")
    }

    /// Builds `tool_result.content` from tool-result text, turning image
    /// markers into nested `image` blocks.
    ///
    /// Multimodal preparation normalizes `[IMAGE:<path>]` to a `data:` URI
    /// whenever the provider reports `vision`. Before nested blocks existed the
    /// payload was serialized into a text position and billed as prose — tens of
    /// thousands of tokens the model reads as gibberish rather than as an image.
    ///
    /// References that fail the shared structural check are dropped and counted
    /// in an omission note instead. With no markers at all the original string
    /// is returned unchanged, so the common path is byte-identical to before.
    ///
    /// Block order is text first, then images, matching Anthropic's own
    /// documented example. (The user-message arm emits images first and text
    /// after; the two arms differ, and the ordering rule Anthropic enforces is
    /// about `tool_result` blocks relative to other blocks in a message, not
    /// about text relative to image inside a block list.)
    fn tool_result_content(content: &str) -> ToolResultContent {
        let (cleaned, refs) = crate::multimodal::parse_image_markers(content);
        if refs.is_empty() {
            // The early return still sweeps. An unterminated marker yields zero
            // references and copies its payload verbatim into the cleaned text,
            // so returning here without sweeping would leave raw base64 in a
            // text position on exactly the path that has no references.
            return ToolResultContent::Text(Self::sweep_residual_image_data(content).into_owned());
        }

        let (sources, omitted) = Self::deliverable_image_sources(&refs);
        // Unloadable placeholders stay in `cleaned` as prose and never reach
        // `refs`, so the count only covers references that were recognised and
        // still could not be sent.
        let text = Self::text_with_omission_note(&cleaned, omitted);

        if sources.is_empty() {
            return ToolResultContent::Text(text);
        }

        let mut blocks = Vec::with_capacity(sources.len() + 1);
        if !text.is_empty() {
            blocks.push(ToolResultBlock::Text { text });
        }
        blocks.extend(
            sources
                .into_iter()
                .map(|source| ToolResultBlock::Image { source }),
        );
        ToolResultContent::Blocks(blocks)
    }

    /// Turns image references into deliverable [`ImageSource`]s, returning how
    /// many were rejected by the shared structural check.
    fn deliverable_image_sources(refs: &[String]) -> (Vec<ImageSource>, usize) {
        let mut sources = Vec::new();
        let mut omitted = 0usize;
        for reference in refs {
            match crate::multimodal::split_base64_image_data_uri(
                reference,
                MAX_ENCODED_IMAGE_PAYLOAD_BYTES,
            ) {
                Ok((media_type, payload)) => sources.push(ImageSource {
                    source_type: "base64".to_string(),
                    media_type: media_type.to_ascii_lowercase(),
                    data: payload.to_string(),
                }),
                Err(_) => omitted += 1,
            }
        }
        (sources, omitted)
    }

    /// Sweeps residual raw base64 out of marker-cleaned prose and appends the
    /// omission note when references were rejected.
    ///
    /// The tool arm uses this unconditionally, through
    /// [`Self::tool_result_content`], its only caller: that input is machine
    /// output, where a quoted data URI is far rarer than a truncated
    /// screenshot. The user arm sweeps only marker-carrying text and then calls
    /// [`Self::text_with_note`] directly — see its call site.
    fn text_with_omission_note(cleaned: &str, omitted: usize) -> String {
        Self::text_with_note(&Self::sweep_residual_image_data(cleaned), omitted)
    }

    /// Appends the omission note to text that has already been swept, or that
    /// deliberately was not.
    fn text_with_note(text: &str, omitted: usize) -> String {
        if omitted == 0 {
            return text.to_string();
        }
        let note = Self::image_omission_note(omitted);
        if text.is_empty() {
            note
        } else {
            format!("{text}\n\n{note}")
        }
    }

    /// Replaces every residual `data:<media type>;base64,<payload>` run in
    /// `text` with [`TRUNCATED_DATA_NOTE`].
    ///
    /// `crate::multimodal::parse_image_markers` does not extract an
    /// **unterminated** marker: with no closing `]` it copies the rest of the
    /// string verbatim into the cleaned text and returns no reference at all. A
    /// history truncated mid-marker would otherwise still put raw base64 in a
    /// text position, which is what this adapter must not do.
    ///
    /// The scan starts at the `data:` prefix, never at a bare payload — a
    /// payload whose header was already truncated away is indistinguishable from
    /// prose and is left alone. A run ends at the first character outside the
    /// base64 alphabet and its padding, except that it continues into the next
    /// line when the lines are uniformly wide (see
    /// [`Self::residual_payload_end`]). The whole run including its header is
    /// replaced.
    ///
    /// A swept run is deliberately **not** added to the omission count: the
    /// count means "references that were recognised and could not be sent", and
    /// a truncated marker was never a reference. Keeping it out of the count is
    /// what stops the sweep from double-reporting.
    ///
    /// On the tool arm, prose that legitimately quotes a data URI is
    /// rewritten too. That is accepted: a documentation-style example in a tool
    /// result is far rarer than a truncated screenshot, and the replacement says
    /// what happened. The user arm does **not** accept that trade — a person
    /// asking what a data URI decodes to must keep their own text — so it runs
    /// this only on marker-carrying messages, where residue is possible at all.
    ///
    /// **What this does not cover**, stated so a reader does not credit it with
    /// more than it does:
    ///
    /// - A header whose media type holds a non-ASCII or otherwise implausible
    ///   character, such as `data:imagé/png;base64,…`. Such a header cannot come
    ///   from this crate's preparation code, and loosening the header rule would
    ///   let any `data:` in prose claim a `;base64,` further down the string.
    /// - Assistant message text. This runs on the tool-result arm
    ///   unconditionally and on the user arm only for marker-carrying messages.
    ///   Assistant content is copied to the wire verbatim, so a data URI the
    ///   model itself wrote is left as the model wrote it.
    /// - A bare data URI a user typed with no image marker anywhere in the
    ///   message. Nothing normalized it, so nothing is residual; it is the
    ///   author's own text and is delivered as written.
    /// - The last, shorter line of a wrapped payload, and the tail of a run that
    ///   is not uniformly wrapped. See [`Self::residual_payload_end`].
    ///
    /// The whole pass is linear in the length of `text`, and deliberately so:
    /// tool output is untrusted, and an earlier version that restarted a search
    /// for `;base64,` from every rejected `data:` cost minutes of CPU on a
    /// one-megabyte input that repeats `data:`.
    fn sweep_residual_image_data(text: &str) -> Cow<'_, str> {
        const SCHEME: &str = "data:";

        let mut out: Option<String> = None;
        // Everything before `copied` is already in `out`; `scan` is where the
        // next search for `data:` starts. They differ whenever a `data:` was
        // examined and left alone.
        let mut copied = 0usize;
        let mut scan = 0usize;

        while let Some(relative) = text[scan..].find(SCHEME) {
            let start = scan + relative;
            let header_start = start + SCHEME.len();
            // Walk the header forward rather than searching ahead for
            // `;base64,`: a real header holds only media-type and parameter
            // characters, so it either ends at `;base64,` or this `data:` is
            // prose. Walking keeps the pass linear.
            let header_end =
                header_start + Self::run_len(&text[header_start..], is_data_uri_header_char);
            // `base64` may sit anywhere in the parameter list, which is what
            // `crate::multimodal::split_base64_image_data_uri` accepts. Requiring
            // it last would leave `data:image/png;base64;charset=x,<payload>`
            // unswept while the same header is delivered as an image elsewhere.
            let mut header_parts = text[header_start..header_end].split(';');
            let plausible_header = header_parts
                .next()
                .is_some_and(|media_type| !media_type.is_empty())
                && header_parts.any(|parameter| parameter == "base64")
                && text[header_end..].starts_with(',');
            if !plausible_header {
                // Resume at the character after `data:`, not at `header_end`: the
                // four letters of `data` are header-legal, so a second `data:`
                // can start inside the header run just walked and jumping past it
                // would let `data:data:image/png;base64,<payload>` through
                // untouched. `data:` has no proper prefix that is also a suffix,
                // so occurrences are at least five bytes apart and resuming here
                // skips nothing.
                //
                // This stays linear. A header walk can only reach the `:` of the
                // next `data:`, so the walks starting inside one another sum to
                // at most the length of `text`.
                scan = header_start;
                continue;
            }

            let payload_start = header_end + 1;
            let end = Self::residual_payload_end(text, payload_start);

            let buffer = out.get_or_insert_with(|| String::with_capacity(text.len()));
            buffer.push_str(&text[copied..start]);
            buffer.push_str(TRUNCATED_DATA_NOTE);
            copied = end;
            scan = end;
        }

        match out {
            Some(mut buffer) => {
                buffer.push_str(&text[copied..]);
                Cow::Owned(buffer)
            }
            None => Cow::Borrowed(text),
        }
    }

    /// Byte length of the leading run of characters satisfying `allowed`.
    fn run_len(text: &str, allowed: fn(char) -> bool) -> usize {
        text.find(|ch: char| !allowed(ch)).unwrap_or(text.len())
    }

    /// End of a residual base64 run that starts at `payload_start`.
    ///
    /// A run normally ends at the first character outside the base64 alphabet.
    /// It continues into the next line only when the text is **uniformly
    /// line-wrapped**, which is what a wrapped payload looks like and what a list
    /// of long tokens does not. `crate::multimodal::parse_image_markers` only
    /// collapses a wrapped marker when it is terminated, so a payload that was
    /// line-wrapped and then truncated arrives with its newlines intact, and
    /// stopping at the first newline would leave every line but the first sitting
    /// in a text position — tens of thousands of prose tokens, which is the
    /// original bug.
    ///
    /// The continuation rule, in full. The gap between two segments must be
    /// exactly one line terminator — `\n`, `\r\n` or `\r` — so a space, an
    /// indent or a blank line ends the run. The width of the first continued
    /// line becomes the wrap width, and it is only accepted as a wrap width when
    /// either the payload's own first line is exactly that wide (a payload
    /// pre-wrapped by an encoder and then prefixed with a marker) or the whole
    /// line the header sits on is exactly that wide (text wrapped as a whole).
    /// Every later line must then match the same width, and the width itself must
    /// be at least [`WRAPPED_BASE64_WIDTH_MIN`].
    ///
    /// That is what keeps real tool output out of the run. A `sha256sum` listing
    /// after a quoted data URI has 64-character lines, but the header's line is
    /// not 64 characters wide and the payload's own first line is not either, so
    /// the run stops at the first newline and the digests survive. An earlier
    /// version continued across any whitespace into any segment of 64 or more
    /// base64 characters, which both ate such listings and missed every wrap
    /// width below 64.
    ///
    /// Two residues are accepted. The last line of a wrapped payload is shorter
    /// than the wrap width, so it stays in the text — at most a wrap width of
    /// base64 characters, sitting next to the note that says data was removed.
    /// Absorbing it would mean deleting whatever short word happens to follow a
    /// quoted data URI. And a payload wrapped with an indent on its continuation
    /// lines is not rejoined.
    fn residual_payload_end(text: &str, payload_start: usize) -> usize {
        let end = Self::residual_payload_run_end(text, payload_start);
        Self::without_trailing_scheme_prefix(text, payload_start, end)
    }

    /// Backs `end` off a trailing `data` when the byte at `end` is the `:` of
    /// another `data:`, so the overlapping occurrence is still examined.
    ///
    /// Every letter of `data` is in the base64 alphabet, so a payload run
    /// swallows the scheme name of a following data URI and stops at its colon.
    /// Resuming the scan there skipped the overlap entirely and left
    /// `:<media type>;base64,<payload>` in a text position. Ending the run before
    /// the scheme name instead moves the cursor *forward* from the run's start,
    /// so the sweep still advances and stays linear.
    fn without_trailing_scheme_prefix(text: &str, payload_start: usize, end: usize) -> usize {
        const SCHEME_NAME: &str = "data";

        if !text[end..].starts_with(':') {
            return end;
        }
        let Some(candidate) = end.checked_sub(SCHEME_NAME.len()) else {
            return end;
        };
        // Never reach back into the header: only payload bytes may be given up.
        if candidate < payload_start || &text[candidate..end] != SCHEME_NAME {
            return end;
        }
        candidate
    }

    /// End of the base64 run itself, before the overlap adjustment in
    /// [`Self::residual_payload_end`].
    fn residual_payload_run_end(text: &str, payload_start: usize) -> usize {
        let first_end =
            payload_start + Self::run_len(&text[payload_start..], is_base64_payload_char);
        let first_len = first_end - payload_start;
        let mut end = first_end;
        let mut wrap_width: Option<usize> = None;

        loop {
            let rest = &text[end..];
            let Some(gap) = Self::line_terminator_len(rest) else {
                // The run ended at punctuation, at a space, or at the end of the
                // string — not at a line break.
                return end;
            };
            let segment = Self::run_len(&rest[gap..], is_base64_payload_char);
            match wrap_width {
                Some(width) if segment == width => {}
                Some(_) => return end,
                None => {
                    // First continuation: decide whether this is wrapping at all.
                    if segment < WRAPPED_BASE64_WIDTH_MIN
                        || (segment != first_len
                            && !Self::line_ends_at_with_width(text, end, segment))
                    {
                        return end;
                    }
                    wrap_width = Some(segment);
                }
            }
            end += gap + segment;
        }
    }

    /// Length of the single line terminator at the start of `text`, or `None`
    /// when `text` does not start with exactly one.
    fn line_terminator_len(text: &str) -> Option<usize> {
        if let Some(rest) = text.strip_prefix('\r') {
            return Some(if rest.starts_with('\n') { 2 } else { 1 });
        }
        if text.starts_with('\n') {
            return Some(1);
        }
        None
    }

    /// True when the line ending at byte index `line_end` is exactly `width`
    /// bytes long.
    ///
    /// Byte length, not character count: a line holding a multi-byte character
    /// is simply not recognised as a wrapped line, which costs nothing but a
    /// sweep this function was never able to justify.
    fn line_ends_at_with_width(text: &str, line_end: usize, width: usize) -> bool {
        if line_end < width {
            return false;
        }
        let line_start = line_end - width;
        if line_start > 0 && !matches!(text.as_bytes()[line_start - 1], b'\n' | b'\r') {
            return false;
        }
        // `get` rejects a boundary that falls inside a multi-byte character.
        text.get(line_start..line_end)
            .is_some_and(|line| !line.contains(['\n', '\r']))
    }

    /// Splits a native tool-result envelope into its `tool_use_id` and result
    /// text. `None` when the message is not such an envelope, which sends the
    /// caller down the non-JSON carrier path with the raw message.
    ///
    /// The presence of a `tool_call_id` key is what identifies the envelope.
    /// Requiring it means a tool that happens to return a JSON object with a
    /// `content` field keeps all of its fields, while an envelope whose id is
    /// unusable still gives up its payload instead of putting
    /// `{"tool_call_id":null,…}` in front of the model as if the tool had
    /// written it.
    ///
    /// A `content` value that is not a string is rendered as its JSON text
    /// rather than treated as absent, in **both** branches. A tool that returns
    /// a structured result meant that object to reach the model; dropping it
    /// left an empty `tool_result` on the wire with nothing saying so, and on the
    /// unusable-id branch it put the envelope scaffolding in front of the model
    /// instead. With no `content` key at all there is no payload, and the caller
    /// treats empty content as a message to skip.
    fn parse_tool_result_envelope(content: &str) -> Option<ToolResultEnvelope> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let object = value.as_object()?;
        let id_field = object.get("tool_call_id")?;
        let result = object
            .get("content")
            .filter(|payload| !payload.is_null())
            .map(|payload| match payload.as_str() {
                Some(text) => text.to_string(),
                None => payload.to_string(),
            });
        Some(ToolResultEnvelope {
            tool_use_id: id_field.as_str().map(str::to_string),
            content: result.unwrap_or_default(),
        })
    }

    fn convert_messages(messages: &[ChatMessage]) -> (Option<SystemPrompt>, Vec<NativeMessage>) {
        let mut system_text = None;
        let mut native_messages = Vec::new();
        let mut run = ToolResultRun::default();

        for (index, msg) in messages.iter().enumerate() {
            if ChatMessage::should_skip_internal_pruning_marker(messages, index) {
                continue;
            }
            match msg.role.as_str() {
                "system" => {
                    // A system message is not emitted into the message list — it
                    // becomes the request's `system` field, or is dropped — so it
                    // cannot break the adjacency between a `tool_use` and its
                    // `tool_result` on the wire, which is the adjacency the
                    // candidate set exists to protect. It therefore does not end
                    // a tool-result run. The same holds for the other messages
                    // that produce no wire content: an empty assistant message
                    // and a skipped pruning marker.
                    if system_text.is_none() {
                        system_text = Some(msg.content.clone());
                    }
                }
                "assistant" => {
                    if let Some(blocks) = Self::parse_assistant_tool_call_message(&msg.content) {
                        run.begin(
                            blocks
                                .iter()
                                .filter_map(|block| match block {
                                    NativeContentOut::ToolUse { id, .. } => Some(id.clone()),
                                    _ => None,
                                })
                                .collect(),
                        );
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: blocks,
                        });
                    } else if !msg.content.trim().is_empty() {
                        // An assistant message without tool calls ends the run.
                        run.end();
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: vec![NativeContentOut::Text {
                                text: msg.content.clone(),
                                cache_control: None,
                            }],
                        });
                    }
                }
                "tool" => {
                    let envelope = Self::parse_tool_result_envelope(&msg.content);
                    // The tool's own output: the envelope's `content` when this
                    // is an envelope at all, the raw message otherwise.
                    let carrier = match &envelope {
                        Some(parsed) => parsed.content.as_str(),
                        None => msg.content.as_str(),
                    };
                    let tool_msg = if let Some(tool_use_id) = envelope
                        .as_ref()
                        .and_then(|parsed| parsed.tool_use_id.clone())
                    {
                        run.mark_answered(&tool_use_id);
                        Self::tool_result_message(tool_use_id, carrier)
                    } else if carrier.trim().is_empty() {
                        // No payload and no usable id: there is nothing to
                        // deliver, so the call is left to the backfill below.
                        continue;
                    } else if let Some(tool_use_id) = run.only_unanswered() {
                        // Non-JSON tool carrier: `ChatMessage::tool` accepts any
                        // string, and an envelope with a non-string
                        // `tool_call_id` lands here too. The id is recovered only
                        // when the assistant turn this message still follows left
                        // exactly one call unanswered — history proves the
                        // association there. That also stops
                        // `backfill_orphaned_tool_uses` from putting a "tool
                        // result missing" stub next to the real result.
                        run.mark_answered(&tool_use_id);
                        Self::tool_result_message(tool_use_id, carrier)
                    } else {
                        // Zero candidates, or two or more: nothing proves which
                        // call this answers. A `tool_result` structurally requires
                        // a `tool_use_id`, so the payload is dropped rather than
                        // emitted as top-level blocks — a top-level block in a
                        // user-role message reads to the model as something the
                        // user wrote, which turns untrusted tool output into
                        // user-authored instruction. The open calls are recorded
                        // so their stubs can say a result arrived and was not
                        // delivered, and the drop is logged.
                        let candidate_count = run.record_undelivered();
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Reject
                            )
                            .with_category(::zeroclaw_log::EventCategory::Provider)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(Self::dropped_output_attrs(candidate_count, carrier.len())),
                            "anthropic: unpairable tool output dropped — no unambiguous tool_use to attach it to"
                        );
                        continue;
                    };
                    // Tool results map to role "user"; merge consecutive ones
                    // into a single message so Anthropic doesn't reject the
                    // request for having adjacent same-role messages. This merge
                    // stays even though `merge_adjacent_same_role` sweeps the
                    // finished list for the same thing: `dedupe_tool_results_by_id`
                    // only sees duplicates that already share a message, and this
                    // is what puts them there.
                    if native_messages
                        .last()
                        .is_some_and(|m| m.role == tool_msg.role)
                    {
                        native_messages
                            .last_mut()
                            .unwrap()
                            .content
                            .extend(tool_msg.content);
                    } else {
                        native_messages.push(tool_msg);
                    }
                }
                _ => {
                    // A user message ends the tool-result run, so a later
                    // non-JSON tool message must not be paired with the assistant
                    // turn before it.
                    run.end();

                    // Parse image markers from user message content
                    let (text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
                    let mut content_blocks: Vec<NativeContentOut> = Vec::new();
                    let mut omitted = 0usize;

                    // Add image content blocks for each image reference
                    for img_ref in &image_refs {
                        let (media_type, data) = if img_ref.starts_with("data:") {
                            // Routed through the same shared structural check the
                            // tool arm uses, so both arms agree on what a
                            // deliverable image is. Stricter than the old
                            // split-on-first-comma: a header without `;base64`, a
                            // media type off the allowlist, a non-canonical
                            // payload, or one over the per-image ceiling is now
                            // skipped here instead of drawing a 400 from the API.
                            match crate::multimodal::split_base64_image_data_uri(
                                img_ref,
                                MAX_ENCODED_IMAGE_PAYLOAD_BYTES,
                            ) {
                                Ok((mime, payload)) => {
                                    (mime.to_ascii_lowercase(), payload.to_string())
                                }
                                Err(_) => {
                                    omitted += 1;
                                    continue;
                                }
                            }
                        } else if std::path::Path::new(img_ref.trim()).exists() {
                            // Local file path
                            match std::fs::read(img_ref.trim()) {
                                Ok(bytes) => {
                                    let b64 =
                                        base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    let ext = std::path::Path::new(img_ref.trim())
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .unwrap_or("jpg");
                                    let mime = match ext {
                                        "png" => "image/png",
                                        "gif" => "image/gif",
                                        "webp" => "image/webp",
                                        _ => "image/jpeg",
                                    }
                                    .to_string();
                                    (mime, b64)
                                }
                                Err(_) => {
                                    omitted += 1;
                                    continue;
                                }
                            }
                        } else {
                            omitted += 1;
                            continue;
                        };

                        content_blocks.push(NativeContentOut::Image {
                            source: ImageSource {
                                source_type: "base64".to_string(),
                                media_type,
                                data,
                            },
                        });
                    }

                    // The sweep runs only on marker-carrying text here. Residual
                    // base64 in a user message can only come from this crate's
                    // own marker normalization, so a message with no marker at
                    // all has nothing residual in it — and sweeping anyway
                    // deleted a data URI the author quoted on purpose ("what does
                    // this data:application/json;base64,… decode to?"). The tool
                    // arm still sweeps unconditionally: its input is machine
                    // output, not something a person typed.
                    //
                    // The test is on `text`, the *cleaned* content, not on
                    // `msg.content`. `parse_image_markers` lifts a loadable marker
                    // out whole, so its payload is already gone from `text` and
                    // its prefix with it; only the two shapes that can leave
                    // residue — an unterminated marker, and a terminated one whose
                    // reference will not load — are copied through verbatim, and
                    // both keep the `[IMAGE:` prefix. Testing the raw message
                    // instead opened the gate for the entire text of any message
                    // that merely attached an image, so a quoted data URI sitting
                    // beside a working attachment was swept — the exact case this
                    // gate exists to prevent, one message shape over.
                    let swept = if crate::multimodal::carries_image_marker(&text) {
                        Self::sweep_residual_image_data(&text)
                    } else {
                        Cow::Borrowed(text.as_str())
                    };
                    // Every reference that produced no block is counted, so a
                    // message whose images were all rejected says so instead of
                    // serializing as empty content.
                    let text = Self::text_with_note(&swept, omitted);

                    // The `[image]` placeholder is gated on a block having been
                    // built, not on references existing: after the stricter
                    // validation above a reference can be present and produce
                    // nothing, and telling the model an image is attached with
                    // none on the wire is worse than saying nothing.
                    if text.is_empty() && !content_blocks.is_empty() {
                        content_blocks.push(NativeContentOut::Text {
                            text: IMAGE_ONLY_TEXT_PLACEHOLDER.to_string(),
                            cache_control: None,
                        });
                    } else if !text.trim().is_empty() {
                        content_blocks.push(NativeContentOut::Text {
                            text,
                            cache_control: None,
                        });
                    }

                    // Merge into previous user message if present (e.g.
                    // when a user message immediately follows tool results
                    // which are also role "user" in Anthropic's format).
                    if native_messages.last().is_some_and(|m| m.role == "user") {
                        native_messages
                            .last_mut()
                            .unwrap()
                            .content
                            .extend(content_blocks);
                    } else {
                        native_messages.push(NativeMessage {
                            role: "user".to_string(),
                            content: content_blocks,
                        });
                    }
                }
            }
        }

        Self::merge_adjacent_same_role(&mut native_messages);
        Self::dedupe_tool_results_by_id(&mut native_messages);
        Self::order_tool_results_first(&mut native_messages);
        Self::backfill_orphaned_tool_uses(&mut native_messages, run.undelivered_ids());

        // Always use Blocks format with cache_control for system prompts
        let system_prompt = system_text.map(|text| {
            SystemPrompt::Blocks(vec![SystemBlock {
                block_type: "text".to_string(),
                text,
                cache_control: Some(CacheControl::ephemeral()),
            }])
        });

        (system_prompt, native_messages)
    }

    /// The attributes of the warning the `"tool"` arm of
    /// [`Self::convert_messages`] logs when it drops an unpairable tool payload.
    ///
    /// `candidate_count` is how many unanswered calls the drop could not choose
    /// between — zero when the assistant turn left none open. `payload_bytes` is
    /// the carrier's length. Those two keys are the stable contract, and the
    /// carrier itself is deliberately absent: it is untrusted tool output and can
    /// be enormous, and the log file is a second place it must not escape to.
    /// Extracted from the call site so a test can pin both facts.
    fn dropped_output_attrs(candidate_count: usize, payload_bytes: usize) -> serde_json::Value {
        serde_json::json!({
            "candidate_count": candidate_count,
            "payload_bytes": payload_bytes,
        })
    }

    /// A user-role message carrying one `tool_result` for `tool_use_id`, which is
    /// how Anthropic represents a tool's answer.
    fn tool_result_message(tool_use_id: String, carrier: &str) -> NativeMessage {
        NativeMessage {
            role: "user".to_string(),
            content: vec![NativeContentOut::ToolResult {
                tool_use_id,
                content: Self::tool_result_content(carrier),
                cache_control: None,
            }],
        }
    }

    /// Keep at most one `tool_result` per `tool_use_id` in each user-role
    /// message, folding any later duplicate into the one that survives.
    ///
    /// Anthropic accepts one `tool_result` per `tool_use`; answering the same
    /// call twice in one message is a 400. That shape is reachable: the non-JSON
    /// carrier recovers the single outstanding id from the assistant turn it
    /// follows, and a JSON envelope naming that same id later in the same run
    /// merges into the same user message. A history restored with the same
    /// envelope twice reaches it too.
    ///
    /// The first block wins, because it is the one adjacent to its `tool_use`.
    /// The later one is neither thrown away nor turned into top-level content:
    /// its text and images are appended inside the surviving `tool_result`,
    /// behind a label naming the call. Tool output is untrusted, and a top-level
    /// block in a user-role message reads to the model as something the user
    /// wrote, so it has to stay within the `tool_result` boundary — see
    /// [`Self::absorb_duplicate_tool_result`] for the rules.
    ///
    /// Runs before [`Self::order_tool_results_first`], which then moves the
    /// surviving `tool_result` blocks back to the front.
    fn dedupe_tool_results_by_id(messages: &mut [NativeMessage]) {
        for message in messages.iter_mut() {
            if message.role == "user" && Self::answers_a_tool_use_twice(message) {
                Self::fold_duplicate_tool_results(message);
            }
        }
    }

    /// Whether a message holds two `tool_result` blocks with the same
    /// `tool_use_id`.
    ///
    /// Checked before the folding walk so the common path — which is every
    /// message this converter normally builds — never reaches it:
    /// `convert_messages` runs over the whole replayed history on every turn.
    /// There are a handful of blocks per message, so a linear scan beats hashing.
    fn answers_a_tool_use_twice(message: &NativeMessage) -> bool {
        let mut ids: Vec<&str> = Vec::new();
        for block in &message.content {
            if let NativeContentOut::ToolResult { tool_use_id, .. } = block {
                if ids.contains(&tool_use_id.as_str()) {
                    return true;
                }
                ids.push(tool_use_id.as_str());
            }
        }
        false
    }

    /// Rebuild a message's content with every repeated `tool_result` folded into
    /// the first block that answered its call.
    fn fold_duplicate_tool_results(message: &mut NativeMessage) {
        let mut kept: Vec<NativeContentOut> = Vec::with_capacity(message.content.len());
        for block in std::mem::take(&mut message.content) {
            match block {
                NativeContentOut::ToolResult {
                    tool_use_id,
                    content,
                    cache_control,
                } => {
                    if Self::answers_tool_use(&kept, &tool_use_id) {
                        Self::absorb_into_answering_tool_result(&mut kept, &tool_use_id, content);
                    } else {
                        kept.push(NativeContentOut::ToolResult {
                            tool_use_id,
                            content,
                            cache_control,
                        });
                    }
                }
                other => kept.push(other),
            }
        }
        message.content = kept;
    }

    /// Whether any of `blocks` is a `tool_result` for `tool_use_id`.
    fn answers_tool_use(blocks: &[NativeContentOut], tool_use_id: &str) -> bool {
        blocks.iter().any(|block| {
            matches!(block, NativeContentOut::ToolResult { tool_use_id: id, .. } if id.as_str() == tool_use_id)
        })
    }

    /// Folds `duplicate` into whichever of `blocks` already answers
    /// `tool_use_id`. A no-op when none does, which the caller rules out by
    /// scanning with [`Self::answers_tool_use`] first.
    fn absorb_into_answering_tool_result(
        blocks: &mut [NativeContentOut],
        tool_use_id: &str,
        duplicate: ToolResultContent,
    ) {
        let retained = blocks.iter_mut().find_map(|block| match block {
            NativeContentOut::ToolResult {
                tool_use_id: id,
                content,
                ..
            } if id.as_str() == tool_use_id => Some(content),
            _ => None,
        });
        if let Some(retained) = retained {
            Self::absorb_duplicate_tool_result(retained, duplicate, tool_use_id);
        }
    }

    /// Merges a duplicate `tool_result`'s payload into the content of the block
    /// that already answered the same call, labelled with the `tool_use_id`.
    ///
    /// Three rules, in order:
    ///
    /// 1. An empty duplicate — text that trims to nothing, or a block list with
    ///    no image and no non-empty text — is dropped, and no label is added.
    ///    There is nothing to attribute.
    /// 2. Both sides text-only: the retained text becomes the retained text, the
    ///    label, and the duplicate's text, newline-separated. `content` stays in
    ///    its untagged string form, so an image-free tool result is still
    ///    byte-identical in shape to what this adapter has always sent, and a
    ///    duplicate cannot silently promote a string to a block list.
    /// 3. Either side carries an image: the retained content is normalized to a
    ///    block list, then the label and the duplicate's blocks are appended in
    ///    the duplicate's original order. An image has no string form, so this is
    ///    the only shape that can hold one.
    ///
    /// A block list may end on an `image`, which top-level content may not:
    /// `apply_cache_to_last_message` writes its breakpoint to the `tool_result`
    /// block itself and never looks inside.
    fn absorb_duplicate_tool_result(
        retained: &mut ToolResultContent,
        duplicate: ToolResultContent,
        tool_use_id: &str,
    ) {
        let duplicate = Self::tool_result_blocks(duplicate);
        if duplicate.is_empty() {
            return;
        }
        let label = format!("{DUPLICATE_TOOL_RESULT_PREFIX} {tool_use_id}]");
        let kept = Self::tool_result_blocks(std::mem::replace(
            retained,
            ToolResultContent::Text(String::new()),
        ));
        *retained = if Self::carries_image(&kept) || Self::carries_image(&duplicate) {
            let mut blocks = kept;
            blocks.push(ToolResultBlock::Text { text: label });
            blocks.extend(duplicate);
            ToolResultContent::Blocks(blocks)
        } else {
            let mut lines = Self::block_texts(kept);
            lines.push(label);
            lines.extend(Self::block_texts(duplicate));
            ToolResultContent::Text(lines.join("\n"))
        };
    }

    /// A tool result's content as nested blocks, dropping text that trims to
    /// nothing so a fold never contributes an empty `text` block.
    fn tool_result_blocks(content: ToolResultContent) -> Vec<ToolResultBlock> {
        let blocks = match content {
            ToolResultContent::Text(text) => vec![ToolResultBlock::Text { text }],
            ToolResultContent::Blocks(blocks) => blocks,
        };
        blocks
            .into_iter()
            .filter(|block| match block {
                ToolResultBlock::Text { text } => !text.trim().is_empty(),
                ToolResultBlock::Image { .. } => true,
            })
            .collect()
    }

    /// Whether any block is an `image`.
    fn carries_image(blocks: &[ToolResultBlock]) -> bool {
        blocks
            .iter()
            .any(|block| matches!(block, ToolResultBlock::Image { .. }))
    }

    /// The text of every `text` block, in order. Only used where neither side
    /// carries an image, so nothing is lost by discarding the other variant.
    fn block_texts(blocks: Vec<ToolResultBlock>) -> Vec<String> {
        blocks
            .into_iter()
            .filter_map(|block| match block {
                ToolResultBlock::Text { text } => Some(text),
                ToolResultBlock::Image { .. } => None,
            })
            .collect()
    }

    /// Move `tool_result` blocks to the front of every user-role message,
    /// preserving relative order within each group.
    ///
    /// Anthropic returns a 400 when text precedes a `tool_result` in the same
    /// user message. This converter merges consecutive tool messages into one
    /// user message and merges a user message into a preceding user-role
    /// message — and a converted tool result *is* a user-role message — so any
    /// user-role text immediately before a tool result lands in the same message
    /// with the text first.
    ///
    /// Runs before [`Self::backfill_orphaned_tool_uses`] so the stub inserter
    /// sees final ordering; the backfill prepends its stubs, so the invariant
    /// still holds afterwards. Assistant messages are untouched.
    fn order_tool_results_first(messages: &mut [NativeMessage]) {
        for message in messages.iter_mut() {
            if message.role == "user" {
                Self::move_tool_results_first(message);
            }
        }
    }

    /// Hoist one message's `tool_result` blocks ahead of every other block,
    /// preserving relative order within each group. A no-op when they already are.
    fn move_tool_results_first(message: &mut NativeMessage) {
        let out_of_order = message
            .content
            .iter()
            .skip_while(|block| matches!(block, NativeContentOut::ToolResult { .. }))
            .any(|block| matches!(block, NativeContentOut::ToolResult { .. }));
        if !out_of_order {
            return;
        }
        let (tool_results, others): (Vec<NativeContentOut>, Vec<NativeContentOut>) =
            std::mem::take(&mut message.content)
                .into_iter()
                .partition(|block| matches!(block, NativeContentOut::ToolResult { .. }));
        message.content = tool_results.into_iter().chain(others).collect();
    }

    /// Concatenate the content of any two neighbouring messages that share a role.
    ///
    /// Anthropic rejects a request whose roles do not alternate, which is the same
    /// rule the local merge in the `"tool"` arm of [`Self::convert_messages`]
    /// serves — but that merge only ever sees the message it is about to push, so
    /// it cannot cover a message the converter emits nothing for. The `"tool"` arm
    /// drops an unpairable payload without pushing anything, and a history like
    /// `user, assistant("thinking"), tool("stray output"), assistant("done")` then
    /// leaves the two assistant messages side by side. No stub rescues that: there
    /// is no `tool_use` anywhere for [`Self::backfill_orphaned_tool_uses`] to
    /// answer. Two plain assistant messages in a row reach the same shape without
    /// any tool message at all.
    ///
    /// Runs before [`Self::dedupe_tool_results_by_id`] and
    /// [`Self::order_tool_results_first`], so a merged message is still subject to
    /// the two per-message invariants they enforce. It does not need to run again
    /// after the backfill: the backfill either prepends its stubs into a following
    /// user message, which changes no roles, or inserts a user message between the
    /// assistant turn that made the call and a message that is not user-role, so
    /// it cannot create an adjacent pair.
    ///
    /// Two adjacent assistant messages are left alone when the earlier one holds
    /// `tool_use` blocks, because the backfill separates that pair anyway and does
    /// it better. Merging there would concatenate the two turns' blocks, which
    /// both flattens two sequential rounds of calls into one apparently parallel
    /// round and leaves the later turn's `thinking` behind the earlier turn's
    /// `tool_use` — and Anthropic requires an assistant message to *start* with
    /// `thinking` when extended thinking is on, see
    /// [`Self::parse_assistant_tool_call_message`], so the merge would have traded
    /// one 400 for another.
    ///
    /// Skipping cannot leave the pair adjacent on the wire. The next message is an
    /// assistant one, so it carries no `tool_result`; every `tool_use` in the
    /// earlier message is therefore unanswered, and
    /// [`Self::backfill_orphaned_tool_uses`] inserts a user message of stubs
    /// between them. The condition here is the same one that makes its `pending`
    /// list non-empty, so wherever this declines to merge, that insert happens.
    ///
    /// This is why the case the merge does have to cover is the one in the
    /// paragraph above: plain assistant messages, with no `tool_use` for the
    /// backfill to answer and no `thinking` to put out of order.
    fn merge_adjacent_same_role(messages: &mut Vec<NativeMessage>) {
        let mut idx = 1;
        while idx < messages.len() {
            if messages[idx].role != messages[idx - 1].role
                || Self::backfill_will_separate(&messages[idx - 1])
            {
                idx += 1;
                continue;
            }
            let merged = messages.remove(idx);
            messages[idx - 1].content.extend(merged.content);
        }
    }

    /// Whether [`Self::backfill_orphaned_tool_uses`] will insert a user message
    /// after `message`, given that the message following it is not user-role.
    ///
    /// True exactly when the message holds a `tool_use`, which is what fills the
    /// backfill's `pending` list.
    fn backfill_will_separate(message: &NativeMessage) -> bool {
        message
            .content
            .iter()
            .any(|block| matches!(block, NativeContentOut::ToolUse { .. }))
    }

    /// Pair any orphaned `tool_use` with a stub `tool_result` so interrupted
    /// turns can't wedge the session with a hard 400 on replay. Defensive
    /// backstop for the canonical-history guard in the runtime.
    ///
    /// Each stub's wording comes from `undelivered`, the set of calls whose result
    /// arrived and was dropped for want of an unambiguous owner: those say so, and
    /// every other call says the turn was interrupted. Getting that wrong would
    /// tell the model a tool never finished when in fact its answer was withheld.
    fn backfill_orphaned_tool_uses(
        messages: &mut Vec<NativeMessage>,
        undelivered: &std::collections::HashSet<String>,
    ) {
        let mut idx = 0;
        while idx < messages.len() {
            let pending: Vec<String> = messages[idx]
                .content
                .iter()
                .filter_map(|block| match block {
                    NativeContentOut::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();

            if pending.is_empty() {
                idx += 1;
                continue;
            }

            let answered: std::collections::HashSet<String> = messages
                .get(idx + 1)
                .map(|next| {
                    next.content
                        .iter()
                        .filter_map(|block| match block {
                            NativeContentOut::ToolResult { tool_use_id, .. } => {
                                Some(tool_use_id.clone())
                            }
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            let stubs: Vec<NativeContentOut> = pending
                .into_iter()
                .filter(|id| !answered.contains(id))
                .map(|tool_use_id| Self::orphan_tool_result_stub(tool_use_id, undelivered))
                .collect();

            if !stubs.is_empty() {
                if messages
                    .get(idx + 1)
                    .is_some_and(|next| next.role == "user")
                {
                    let next = &mut messages[idx + 1];
                    let mut merged = stubs;
                    merged.append(&mut next.content);
                    next.content = merged;
                } else {
                    messages.insert(
                        idx + 1,
                        NativeMessage {
                            role: "user".to_string(),
                            content: stubs,
                        },
                    );
                }
            }

            idx += 1;
        }
    }

    /// The stub `tool_result` for one unanswered call, worded for why it is
    /// unanswered.
    fn orphan_tool_result_stub(
        tool_use_id: String,
        undelivered: &std::collections::HashSet<String>,
    ) -> NativeContentOut {
        let text = if undelivered.contains(&tool_use_id) {
            UNDELIVERED_TOOL_RESULT_STUB
        } else {
            INTERRUPTED_TOOL_RESULT_STUB
        };
        NativeContentOut::ToolResult {
            tool_use_id,
            content: ToolResultContent::Text(text.to_string()),
            cache_control: None,
        }
    }

    fn parse_native_response(response: NativeChatResponse) -> ProviderChatResponse {
        let mut text_parts = Vec::new();
        let mut thinking_parts = Vec::new();
        let mut tool_calls = Vec::new();

        let usage = response.usage.map(|u| {
            let uncached = u.input_tokens.unwrap_or(0);
            let cache_read = u.cache_read_input_tokens.unwrap_or(0);
            let cache_create = u.cache_creation_input_tokens.unwrap_or(0);
            let total = uncached
                .saturating_add(cache_read)
                .saturating_add(cache_create);
            let any_reported = u.input_tokens.is_some()
                || u.cache_read_input_tokens.is_some()
                || u.cache_creation_input_tokens.is_some();
            TokenUsage {
                input_tokens: if any_reported { Some(total) } else { None },
                output_tokens: u.output_tokens,
                cached_input_tokens: u.cache_read_input_tokens,
            }
        });

        for block in response.content {
            match block.kind.as_str() {
                "text" => {
                    if let Some(text) = block.text.map(|t| t.trim().to_string())
                        && !text.is_empty()
                    {
                        text_parts.push(text);
                    }
                }
                "thinking" => {
                    if let Some(thinking) = block.thinking.as_deref().or(block.text.as_deref())
                        && !thinking.is_empty()
                    {
                        let json_block = serde_json::json!({
                            "thinking": thinking,
                            "signature": block.signature.as_deref().unwrap_or(""),
                        });
                        thinking_parts.push(json_block.to_string());
                    }
                }
                "tool_use" => {
                    let name = block.name.unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let arguments = block
                        .input
                        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                    tool_calls.push(ProviderToolCall {
                        id: block.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: arguments.to_string(),
                        extra_content: None,
                    });
                }
                _ => {}
            }
        }

        let reasoning_content = if thinking_parts.is_empty() {
            None
        } else {
            Some(thinking_parts.join("\n"))
        };

        ProviderChatResponse {
            text: if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            },
            tool_calls,
            usage,
            reasoning_content,
        }
    }

    /// Resolve thinking parameters for an API request. Returns the effective
    /// temperature (forced to 1.0 when thinking is active), the thinking
    /// config for the request body, and the effective max_tokens (raised to
    /// meet budget_tokens minimum when needed).
    fn resolve_thinking(
        &self,
        thinking: Option<zeroclaw_api::model_provider::NativeThinkingParams>,
        temperature: Option<f64>,
        model: &str,
    ) -> (Option<f64>, Option<NativeThinkingConfig>, u32) {
        match thinking {
            Some(params) if anthropic_model_supports_native_thinking(model) => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"budget_tokens": params.budget_tokens})),
                    "Native extended thinking enabled; forcing temperature=1.0"
                );
                // API requires max_tokens > budget_tokens (strictly greater).
                let min_required = params.budget_tokens + 1;
                let max_tokens = self.max_tokens.max(min_required);
                (
                    Some(1.0),
                    Some(NativeThinkingConfig {
                        kind: "enabled",
                        budget_tokens: params.budget_tokens,
                    }),
                    max_tokens,
                )
            }
            Some(_) => {
                // Caller asked for native thinking but the model rejects the
                // fixed-budget request shape. Drop to prompt-based reasoning
                // (the agent loop's prefix already injected) and keep the
                // caller-supplied temperature so per-model guards still apply.
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"model": model})),
                    "Native extended thinking requested but model only supports adaptive thinking; falling back to prompt-based reasoning"
                );
                (temperature, None, self.max_tokens)
            }
            None => (temperature, None, self.max_tokens),
        }
    }

    fn http_client(&self) -> Client {
        zeroclaw_config::schema::build_runtime_proxy_client_with_timeouts(
            "model_provider.anthropic",
            self.timeout_secs,
            10,
        )
    }

    /// Streaming requests have no whole-request deadline. Header acquisition
    /// and buffered error bodies are bounded separately, while successful SSE
    /// bodies use the shared byte-idle timeout.
    fn streaming_http_client(&self) -> Result<Client, reqwest::Error> {
        let builder = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(STREAM_IDLE_TIMEOUT);
        let builder = zeroclaw_config::schema::apply_runtime_proxy_to_builder(
            builder,
            "model_provider.anthropic",
        );
        builder.build()
    }

    /// Build a streaming request body from a `NativeChatRequest`.
    fn build_streaming_request(request: &NativeChatRequest) -> anyhow::Result<serde_json::Value> {
        let mut body = serde_json::to_value(request)
            .context("Failed to serialize NativeChatRequest to JSON")?;
        body["stream"] = serde_json::Value::Bool(true);
        Ok(body)
    }

    /// Parse Anthropic SSE lines from `response` and send `StreamEvent`s to `tx`.
    async fn parse_anthropic_sse(
        response: reqwest::Response,
        tx: &tokio::sync::mpsc::Sender<StreamResult<StreamEvent>>,
    ) {
        use tokio_util::io::StreamReader;

        let byte_stream = response
            .bytes_stream()
            .map(|result| result.map_err(std::io::Error::other));
        let reader = StreamReader::new(byte_stream);
        Self::parse_anthropic_sse_from_reader(reader, tx).await;
    }

    /// Inner loop split out of `parse_anthropic_sse` so unit tests can feed a
    /// `Cursor<&[u8]>` directly without spinning up a mock HTTP server.
    async fn parse_anthropic_sse_from_reader<R>(
        reader: R,
        tx: &tokio::sync::mpsc::Sender<StreamResult<StreamEvent>>,
    ) where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        use tokio::io::AsyncBufReadExt;

        let mut lines = reader.lines();

        let mut tool_id: Option<String> = None;
        let mut tool_name: Option<String> = None;
        let mut tool_input_json = String::new();

        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;
        let mut cached_input_tokens: Option<u64> = None;
        let mut cache_creation_input_tokens: Option<u64> = None;

        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(err) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Provider)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "error": format!("{err}"),
                            })),
                        "stream: SSE read error — aborting stream"
                    );
                    let _ = tx
                        .send(Err(StreamError::Http(format!("SSE read error: {err}"))))
                        .await;
                    return;
                }
            };
            let line = line.trim().to_string();
            if !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line["data: ".len()..];

            let event: serde_json::Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = event
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or_default();

            match event_type {
                "message_start" => {
                    let model = event
                        .get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    let usage = event.get("message").and_then(|m| m.get("usage"));
                    let observed_input = usage
                        .and_then(|u| u.get("input_tokens"))
                        .and_then(|t| t.as_u64());
                    let observed_cached = usage
                        .and_then(|u| u.get("cache_read_input_tokens"))
                        .and_then(|t| t.as_u64());
                    let observed_cache_create = usage
                        .and_then(|u| u.get("cache_creation_input_tokens"))
                        .and_then(|t| t.as_u64());
                    if let Some(v) = observed_input {
                        input_tokens = Some(v);
                    }
                    if let Some(v) = observed_cached {
                        cached_input_tokens = Some(v);
                    }
                    if let Some(v) = observed_cache_create {
                        cache_creation_input_tokens = Some(v);
                    }
                    ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model": model, "input_tokens": observed_input, "cached_input_tokens": observed_cached, "cache_creation_input_tokens": observed_cache_create})), "stream: message_start");
                }
                "content_block_start" => {
                    if let Some(block) = event.get("content_block") {
                        let block_type = block
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default();
                        if block_type == "tool_use" {
                            if let Some(id) = tool_id.take() {
                                let name = tool_name.take().unwrap_or_default();
                                let input = std::mem::take(&mut tool_input_json);
                                let _ = tx
                                    .send(Ok(StreamEvent::ToolCall(ProviderToolCall {
                                        id,
                                        name,
                                        arguments: input,
                                        extra_content: None,
                                    })))
                                    .await;
                            }
                            tool_id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(ToString::to_string);
                            tool_name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(ToString::to_string);
                            tool_input_json.clear();
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = event.get("delta") {
                        let delta_type = delta
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default();
                        match delta_type {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(|t| t.as_str())
                                    && !text.is_empty()
                                    && tx
                                        .send(Ok(StreamEvent::TextDelta(StreamChunk::delta(
                                            text.to_string(),
                                        ))))
                                        .await
                                        .is_err()
                                {
                                    return;
                                }
                            }
                            "input_json_delta" => {
                                if let Some(json) =
                                    delta.get("partial_json").and_then(|j| j.as_str())
                                {
                                    tool_input_json.push_str(json);
                                }
                            }
                            // TODO: handle "thinking_delta" events for streaming
                            // extended thinking content. Currently thinking blocks
                            // are only captured in non-streaming parse_native_response().
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    if let Some(id) = tool_id.take() {
                        let name = tool_name.take().unwrap_or_default();
                        let input = std::mem::take(&mut tool_input_json);
                        let _ = tx
                            .send(Ok(StreamEvent::ToolCall(ProviderToolCall {
                                id,
                                name,
                                arguments: input,
                                extra_content: None,
                            })))
                            .await;
                    }
                }
                "message_delta" => {
                    let stop_reason = event
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("none");
                    // Anthropic's running-total: each `message_delta`
                    // supersedes the previous one, so we always overwrite.
                    let observed_output = event
                        .get("usage")
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(|t| t.as_u64());
                    if let Some(v) = observed_output {
                        output_tokens = Some(v);
                    }
                    if stop_reason == "max_tokens" {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"output_tokens": observed_output})),
                            "response truncated: hit max_tokens limit. Increase provider_max_tokens in config."
                        );
                    } else {
                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"stop_reason": stop_reason, "output_tokens": observed_output})), "stream: message_delta");
                    }
                }
                "message_stop" => {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        "stream: message_stop"
                    );
                    if input_tokens.is_some()
                        || output_tokens.is_some()
                        || cached_input_tokens.is_some()
                        || cache_creation_input_tokens.is_some()
                    {
                        let uncached = input_tokens.unwrap_or(0);
                        let cache_read = cached_input_tokens.unwrap_or(0);
                        let cache_create = cache_creation_input_tokens.unwrap_or(0);
                        let normalized_input = Some(
                            uncached
                                .saturating_add(cache_read)
                                .saturating_add(cache_create),
                        );
                        let _ = tx
                            .send(Ok(StreamEvent::Usage(TokenUsage {
                                input_tokens: normalized_input,
                                output_tokens,
                                cached_input_tokens,
                            })))
                            .await;
                    }
                    let _ = tx.send(Ok(StreamEvent::Final)).await;
                    return;
                }
                "error" => {
                    let msg = event
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown streaming error");
                    let _ = tx
                        .send(Err(StreamError::ModelProvider(msg.to_string())))
                        .await;
                    return;
                }
                _ => {}
            }
        }

        crate::stream_guard::finish_sse_stream(tx, false, "message_stop").await;
    }
}

#[async_trait]
impl ModelProvider for AnthropicModelProvider {
    fn default_temperature(&self) -> f64 {
        TEMPERATURE_DEFAULT
    }

    fn default_base_url(&self) -> Option<&str> {
        Some(BASE_URL)
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"missing": "credentials"})),
                "anthropic: no credentials configured"
            );
            anyhow::Error::msg(
                "Anthropic credentials not set. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN (setup-token).",
            )
        })?;

        let system = system_prompt.map(|s| SystemPrompt::String(s.to_string()));
        let system = if Self::is_setup_token(credential) {
            Self::apply_oauth_system_prompt(system)
        } else {
            system
        };

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"max_tokens": self.max_tokens, "model": model})),
            "API request"
        );
        let request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: self.max_tokens,
            system,
            messages: vec![NativeMessage {
                role: "user".to_string(),
                content: vec![NativeContentOut::Text {
                    text: message.to_string(),
                    cache_control: None,
                }],
            }],
            temperature,
            tools: None,
            tool_choice: None,
            stream: None,
            thinking: None,
        };

        let mut request = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request);

        request = self.apply_auth(request, credential);

        let response = request.send().await?;

        if !response.status().is_success() {
            return Err(super::api_error("Anthropic", response).await);
        }

        let chat_response: NativeChatResponse = response.json().await?;
        let parsed = Self::parse_native_response(chat_response);
        parsed.text.ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "anthropic: empty text in response"
            );
            anyhow::Error::msg("No response from Anthropic")
        })
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"missing": "credentials"})),
                "anthropic: no credentials configured"
            );
            anyhow::Error::msg(
                "Anthropic credentials not set. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN (setup-token).",
            )
        })?;

        let (system_prompt, mut messages) = Self::convert_messages(request.messages);

        // Auto-cache last message if conversation is long
        if Self::should_cache_conversation(request.messages) {
            Self::apply_cache_to_last_message(&mut messages);
        }

        // Check for tool_choice override from the agent loop (e.g. "any"
        // to force tool use for hardware requests).
        let tool_choice_override = zeroclaw_api::TOOL_CHOICE_OVERRIDE
            .try_with(Clone::clone)
            .ok()
            .flatten();
        let native_tools = self.convert_tools(request.tools);
        let tools_count = native_tools.as_ref().map_or(0, Vec::len);
        let tool_choice = if native_tools.is_some() {
            tool_choice_override.map(|tc| serde_json::json!({ "type": tc }))
        } else {
            None
        };

        // For OAuth tokens, prepend Claude Code identity to system prompt
        let system_prompt = if Self::is_setup_token(credential) {
            Self::apply_oauth_system_prompt(system_prompt)
        } else {
            system_prompt
        };

        let (effective_temperature, thinking_config, effective_max_tokens) =
            self.resolve_thinking(request.thinking, temperature, model);

        if ::zeroclaw_log::debug_enabled() {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "provider": "anthropic",
                        "alias": &self.alias,
                        "request_api": "messages",
                        "model": model,
                        "stream": false,
                        "max_tokens": effective_max_tokens,
                        "tools_count": tools_count,
                        "tool_choice": tool_choice.as_ref().and_then(|value| value.get("type")).and_then(|value| value.as_str()),
                        "thinking_enabled": thinking_config.is_some(),
                    })),
                "anthropic provider request prepared"
            );
        }
        let native_request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: effective_max_tokens,
            system: system_prompt,
            messages,
            temperature: effective_temperature,
            tools: native_tools,
            tool_choice,
            stream: None,
            thinking: thinking_config,
        };

        let req = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&native_request);

        let response = self.apply_auth(req, credential).send().await?;
        if !response.status().is_success() {
            return Err(super::api_error("Anthropic", response).await);
        }

        let native_response: NativeChatResponse = response.json().await?;
        Ok(Self::parse_native_response(native_response))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            prompt_caching: true,
            extended_thinking: true,
        }
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ProviderChatResponse> {
        // Convert OpenAI-format tool JSON to ToolSpec so we can reuse the
        // existing `chat()` method which handles full message history,
        // system prompt extraction, caching, and Anthropic native formatting.
        let tool_specs: Vec<ToolSpec> = tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function").or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "Skipping malformed tool definition (missing 'function' key)"
                    );
                    None
                })?;
                let name = func.get("name").and_then(|n| n.as_str()).or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "Skipping tool with missing or non-string 'name'"
                    );
                    None
                })?;
                Some(ToolSpec::new(
                    name.to_string(),
                    func.get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    func.get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::json!({"type": "object"})),
                ))
            })
            .collect();

        let request = ProviderChatRequest {
            messages,
            tools: if tool_specs.is_empty() {
                None
            } else {
                Some(&tool_specs)
            },
            thinking: None,
        };
        self.chat(request, model, temperature).await
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if let Some(credential) = self.credential.as_ref() {
            let mut request = self
                .http_client()
                .post(format!("{}/v1/messages", self.base_url))
                .header("anthropic-version", "2023-06-01");
            request = self.apply_auth(request, credential);
            // Send a minimal request; the goal is TLS + HTTP/2 setup, not a valid response.
            // Anthropic has no lightweight GET endpoint, so we accept any non-network error.
            let _ = request.send().await?;
        }
        Ok(())
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        // Anthropic's /v1/models requires a credential. Onboard pulls the
        // catalog from models.dev before the user has entered a key.
        crate::models_dev::list_models_for("anthropic").await
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        true
    }

    fn stream_chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamEvent>> {
        if !options.enabled {
            return stream::once(async { Ok(StreamEvent::Final) }).boxed();
        }

        let credential = match self.credential.as_ref() {
            Some(c) => c.clone(),
            None => {
                return stream::once(async {
                    Err(StreamError::ModelProvider(
                        "Anthropic credentials not set".to_string(),
                    ))
                })
                .boxed();
            }
        };

        let (system_prompt, mut messages) = Self::convert_messages(request.messages);
        if Self::should_cache_conversation(request.messages) {
            Self::apply_cache_to_last_message(&mut messages);
        }

        let tool_choice_override = zeroclaw_api::TOOL_CHOICE_OVERRIDE
            .try_with(Clone::clone)
            .ok()
            .flatten();
        let native_tools = self.convert_tools(request.tools);
        let tools_count = native_tools.as_ref().map_or(0, Vec::len);
        let tool_choice = if native_tools.is_some() {
            tool_choice_override.map(|tc| serde_json::json!({ "type": tc }))
        } else {
            None
        };

        let system_prompt = if Self::is_setup_token(&credential) {
            Self::apply_oauth_system_prompt(system_prompt)
        } else {
            system_prompt
        };

        let (effective_temperature, thinking_config, effective_max_tokens) =
            self.resolve_thinking(request.thinking, temperature, model);

        if thinking_config.is_some() {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "provider": "anthropic",
                        "alias": &self.alias,
                        "request_api": "messages",
                        "model": model,
                        "stream": false,
                        "tools_count": tools_count,
                        "tool_choice": tool_choice.as_ref().and_then(|value| value.get("type")).and_then(|value| value.as_str()),
                    })),
                "native thinking enabled; using non-streaming fallback to preserve signed thinking blocks"
            );
            let native_request = NativeChatRequest {
                model: model.to_string(),
                max_tokens: effective_max_tokens,
                system: system_prompt,
                messages,
                temperature: effective_temperature,
                tools: native_tools,
                tool_choice,
                stream: None,
                thinking: thinking_config,
            };
            // Serialize eagerly so the request body is owned and `'static`
            // across the async boundary.
            let body = serde_json::to_value(&native_request)
                .expect("NativeChatRequest should serialize to JSON");
            let client = self.http_client();
            let url = format!("{}/v1/messages", self.base_url);
            let is_oauth = Self::is_setup_token(&credential);

            return stream::once(async move {
                let mut req = client
                    .post(&url)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body);
                if is_oauth {
                    req = req
                        .header("Authorization", format!("Bearer {credential}"))
                        .header(
                            "anthropic-beta",
                            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14",
                        )
                        .header("anthropic-dangerous-direct-browser-access", "true");
                } else {
                    req = req.header("x-api-key", &credential);
                }
                let response = req
                    .send()
                    .await
                    .map_err(|e| StreamError::Http(e.to_string()))?;
                if !response.status().is_success() {
                    let status = response.status();
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| format!("HTTP error: {status}"));
                    return Err(StreamError::ModelProvider(format!("{status}: {body}")));
                }
                let parsed: NativeChatResponse = response
                    .json()
                    .await
                    .map_err(|e| StreamError::ModelProvider(format!("response decode: {e}")))?;
                Ok(Self::parse_native_response(parsed))
            })
            .flat_map(|result| match result {
                Ok(resp) => {
                    let mut events: Vec<StreamResult<StreamEvent>> = Vec::new();
                    if let Some(rc) = resp.reasoning_content {
                        events.push(Ok(StreamEvent::TextDelta(StreamChunk {
                            delta: String::new(),
                            reasoning: Some(rc),
                            is_final: false,
                            token_count: 0,
                        })));
                    }
                    if let Some(text) = resp.text.filter(|t| !t.is_empty()) {
                        events.push(Ok(StreamEvent::TextDelta(StreamChunk::delta(text))));
                    }
                    for tc in resp.tool_calls {
                        events.push(Ok(StreamEvent::ToolCall(tc)));
                    }
                    if let Some(usage) = resp.usage {
                        events.push(Ok(StreamEvent::Usage(usage)));
                    }
                    events.push(Ok(StreamEvent::Final));
                    stream::iter(events)
                }
                Err(e) => stream::iter(vec![Err(e)]),
            })
            .boxed();
        }

        if ::zeroclaw_log::debug_enabled() {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "provider": "anthropic",
                        "alias": &self.alias,
                        "request_api": "messages",
                        "model": model,
                        "stream": true,
                        "max_tokens": effective_max_tokens,
                        "tools_count": tools_count,
                        "tool_choice": tool_choice.as_ref().and_then(|value| value.get("type")).and_then(|value| value.as_str()),
                        "thinking_enabled": false,
                    })),
                "anthropic streaming provider request prepared"
            );
        }
        let native_request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: effective_max_tokens,
            system: system_prompt,
            messages,
            temperature: effective_temperature,
            tools: native_tools,
            tool_choice,
            stream: Some(true),
            thinking: thinking_config,
        };

        let body = match Self::build_streaming_request(&native_request) {
            Ok(body) => body,
            Err(e) => {
                return stream::once(async move { Err(StreamError::ModelProvider(e.to_string())) })
                    .boxed();
            }
        };
        let client = match self.streaming_http_client() {
            Ok(client) => client,
            Err(error) => {
                let message = format!(
                    "Failed to build Anthropic streaming client: {}",
                    super::format_error_chain(&error)
                );
                return stream::once(async move { Err(StreamError::Http(message)) }).boxed();
            }
        };
        let url = format!("{}/v1/messages", self.base_url);
        let is_oauth = Self::is_setup_token(&credential);
        let phase_timeout = std::time::Duration::from_secs(self.timeout_secs);

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamEvent>>(64);

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Spawn)
                .with_category(::zeroclaw_log::EventCategory::Provider)
                .with_attrs(::serde_json::json!({
                    "idle_timeout_secs": STREAM_IDLE_TIMEOUT.as_secs(),
                    "channel_capacity": 64,
                })),
            "stream: spawning detached Anthropic SSE parser task"
        );

        let parser_handle = ::zeroclaw_spawn::spawn!(async move {
            let mut req = client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body);

            if is_oauth {
                req = req
                    .header("Authorization", format!("Bearer {credential}"))
                    .header(
                        "anthropic-beta",
                        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14",
                    )
                    .header("anthropic-dangerous-direct-browser-access", "true");
            } else {
                req = req.header("x-api-key", &credential);
            }

            let response = match tokio::time::timeout(phase_timeout, req.send()).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    let _ = tx
                        .send(Err(StreamError::Http(super::format_error_chain(&e))))
                        .await;
                    return;
                }
                Err(_) => {
                    let _ = tx
                        .send(Err(StreamError::Http(format!(
                            "streaming response headers not received within {}s",
                            phase_timeout.as_secs()
                        ))))
                        .await;
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let error = match tokio::time::timeout(phase_timeout, response.text()).await {
                    Ok(Ok(body)) => body,
                    Ok(Err(error)) => format!("error response body read failed: {error}"),
                    Err(_) => format!(
                        "error response body not received within {}s",
                        phase_timeout.as_secs()
                    ),
                };
                let _ = tx
                    .send(Err(StreamError::ModelProvider(format!(
                        "{status}: {error}"
                    ))))
                    .await;
                return;
            }

            Self::parse_anthropic_sse(response, &tx).await;
        });

        // The guard travels inside the unfold state so it is dropped at the
        // exact moment the consumer drops the stream — turning a turn cancel
        // (or normal completion) into an immediate parser-task abort instead
        // of a leaked socket that lingers until STREAM_IDLE_TIMEOUT.
        let guard = AbortOnDrop::new(parser_handle.abort_handle());
        stream::unfold((rx, guard), |(mut rx, guard)| async move {
            rx.recv().await.map(|event| (event, (rx, guard)))
        })
        .boxed()
    }
}

impl ::zeroclaw_api::attribution::Attributable for AnthropicModelProvider {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Provider(
            ::zeroclaw_api::attribution::ProviderKind::Model(
                ::zeroclaw_api::attribution::ModelProviderKind::Anthropic,
            ),
        )
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(test)]
mod tests;
