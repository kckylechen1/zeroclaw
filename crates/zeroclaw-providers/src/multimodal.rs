use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use zeroclaw_api::model_provider::ChatMessage;
use zeroclaw_config::schema::{MultimodalConfig, build_runtime_proxy_client_with_timeouts};

const IMAGE_MARKER_PREFIX: &str = "[IMAGE:";
const ALLOWED_IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Per-path cache for resolved local image data URIs. Keyed by absolute
/// path; stores `(len, mtime)` for freshness checks (`(0, 0)` sentinel
/// = immutable upload). LRU evicts by both entry count and total bytes.
#[derive(Debug, Default)]
pub struct LocalImageCache {
    entries: HashMap<String, (u64, i64, String)>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

const LOCAL_IMAGE_CACHE_MAX_ENTRIES: usize = 32;
const LOCAL_IMAGE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

impl LocalImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&mut self, path: &str, len: u64, mtime: i64) -> Option<&str> {
        let (cached_len, cached_mtime, _) = self.entries.get(path)?;
        let immutable = *cached_len == 0 && *cached_mtime == 0;
        let fresh = *cached_len == len && *cached_mtime == mtime;
        if !immutable && !fresh {
            return None;
        }
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            let key = self.order.remove(pos).expect("position valid");
            self.order.push_back(key);
        }
        self.entries.get(path).map(|(_, _, uri)| uri.as_str())
    }

    fn insert(&mut self, path: String, len: u64, mtime: i64, data_uri: String) {
        if let Some((_, _, old)) = self.entries.remove(&path) {
            self.bytes = self.bytes.saturating_sub(old.len());
            if let Some(pos) = self.order.iter().position(|p| p == &path) {
                self.order.remove(pos);
            }
        }
        self.bytes += data_uri.len();
        self.entries.insert(path.clone(), (len, mtime, data_uri));
        self.order.push_back(path);
        while self.entries.len() > LOCAL_IMAGE_CACHE_MAX_ENTRIES
            || self.bytes > LOCAL_IMAGE_CACHE_MAX_BYTES
        {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some((_, _, uri)) = self.entries.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(uri.len());
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct PreparedMessages {
    pub messages: Vec<ChatMessage>,
    pub contains_images: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MultimodalError {
    #[error("multimodal image limit exceeded: max_images={max_images}, found={found}")]
    TooManyImages { max_images: usize, found: usize },

    #[error(
        "multimodal image size limit exceeded for '{input}': {size_bytes} bytes > {max_bytes} bytes"
    )]
    ImageTooLarge {
        input: String,
        size_bytes: usize,
        max_bytes: usize,
    },

    #[error("multimodal image MIME type is not allowed for '{input}': {mime}")]
    UnsupportedMime { input: String, mime: String },

    #[error("multimodal remote image fetch is disabled for '{input}'")]
    RemoteFetchDisabled { input: String },

    #[error("multimodal image source not found or unreadable: '{input}'")]
    ImageSourceNotFound { input: String },

    #[error("invalid multimodal image marker '{input}': {reason}")]
    InvalidMarker { input: String, reason: String },

    #[error("failed to download remote image '{input}': {reason}")]
    RemoteFetchFailed { input: String, reason: String },

    #[error("failed to read local image '{input}': {reason}")]
    LocalReadFailed { input: String, reason: String },
}

/// Why a candidate image reference cannot be sent as an inline base64 image
/// block.
///
/// Deliberately a small copy type rather than a [`MultimodalError`]: the
/// checker below runs over the whole replayed conversation on every turn, and
/// an owned error would allocate for every rejected reference on that path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageDataUriRejection {
    /// Not a `data:` URI at all — a filesystem path, an `http(s)` URL, or prose.
    NotADataUri,
    /// A `data:` URI whose header does not declare `;base64`.
    NotBase64Encoded,
    /// Media type outside [`ALLOWED_IMAGE_MIME_TYPES`].
    UnsupportedMediaType,
    /// Payload is empty or is not canonical padded base64.
    MalformedBase64,
    /// Encoded payload exceeds the caller's per-image ceiling.
    TooLarge,
}

impl std::fmt::Display for ImageDataUriRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::NotADataUri => "not a base64 data URI",
            Self::NotBase64Encoded => "data URI is not base64-encoded",
            Self::UnsupportedMediaType => "unsupported image media type",
            Self::MalformedBase64 => "malformed base64 payload",
            Self::TooLarge => "image payload exceeds the per-image ceiling",
        };
        f.write_str(reason)
    }
}

/// Splits a `data:` image reference into its media type and base64 payload,
/// checking the structure without decoding it.
///
/// Both halves of the returned pair borrow from `candidate`; a caller that
/// needs an owned lowercase media type allocates it once when it builds its
/// wire block. `encoded_ceiling` is measured on the **encoded** payload
/// length, unlike `max_bytes` elsewhere in this module, which counts decoded
/// bytes.
///
/// This performs no decoding, no filesystem access and no network I/O on
/// purpose. Provider adapters call it while converting an entire replayed
/// history on every turn, so decoding here would mean re-decoding and
/// re-encoding every image in the conversation once per turn.
///
/// It splits and structurally checks. It does not claim the payload decodes to
/// a real image — nothing short of an image decoder can claim that.
pub(crate) fn split_base64_image_data_uri(
    candidate: &str,
    encoded_ceiling: usize,
) -> Result<(&str, &str), ImageDataUriRejection> {
    let rest = candidate
        .strip_prefix("data:")
        .ok_or(ImageDataUriRejection::NotADataUri)?;
    let Some(comma) = rest.find(',') else {
        return Err(ImageDataUriRejection::NotADataUri);
    };

    let header = &rest[..comma];
    let payload = rest[comma + 1..].trim();

    // Matched case-sensitively, exactly as `normalize_data_uri` does, but on a
    // whole parameter rather than a substring. `contains(";base64")` also
    // accepted `;base64foo`, which the Anthropic adapter's residual sweep
    // declines to sweep because it requires an exact `base64` parameter — so
    // such a header fell between the two and left raw base64 in a text position.
    // The parameter may sit anywhere in the list, which is what the sweep allows.
    if !header
        .split(';')
        .skip(1)
        .any(|parameter| parameter == "base64")
    {
        return Err(ImageDataUriRejection::NotBase64Encoded);
    }

    let media_type = header.split(';').next().unwrap_or_default().trim();
    if !ALLOWED_IMAGE_MIME_TYPES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(media_type))
    {
        return Err(ImageDataUriRejection::UnsupportedMediaType);
    }

    // Checked before the character scan so an oversized payload costs one
    // comparison rather than a full pass.
    if payload.len() > encoded_ceiling {
        return Err(ImageDataUriRejection::TooLarge);
    }

    if !is_canonical_base64_payload(payload) {
        return Err(ImageDataUriRejection::MalformedBase64);
    }

    Ok((media_type, payload))
}

/// True when `payload` is canonical padded base64 in the standard alphabet:
/// non-empty, a multiple of four characters, at most two trailing `=`, and the
/// padding bits of the final quartet zero.
///
/// The final-quartet check is what stops a payload like `AB==` — correct
/// length, legal characters — from passing here and then failing a strict
/// decoder on the provider's side.
fn is_canonical_base64_payload(payload: &str) -> bool {
    if payload.is_empty() || !payload.len().is_multiple_of(4) {
        return false;
    }

    let bytes = payload.as_bytes();
    let pad = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if pad > 2 {
        return false;
    }

    let body = &bytes[..bytes.len() - pad];
    if !body.iter().all(|b| is_standard_base64_char(*b)) {
        return false;
    }

    // `len % 4 == 0` and non-empty means `len >= 4`, so with `pad <= 2` the
    // body always has at least the two characters indexed below.
    match pad {
        // `xyz=` carries 18 bits of payload in 24 bits of encoding: the last
        // character must have its low two bits clear.
        1 => matches!(
            body[body.len() - 1],
            b'A' | b'E'
                | b'I'
                | b'M'
                | b'Q'
                | b'U'
                | b'Y'
                | b'c'
                | b'g'
                | b'k'
                | b'o'
                | b's'
                | b'w'
                | b'0'
                | b'4'
                | b'8'
        ),
        // `xy==` carries 12 bits: the last character must have its low four
        // bits clear.
        2 => matches!(body[body.len() - 1], b'A' | b'Q' | b'g' | b'w'),
        _ => true,
    }
}

fn is_standard_base64_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'
}

fn is_loadable_image_reference(candidate: &str) -> bool {
    candidate.starts_with('/')
        || candidate.starts_with("http://")
        || candidate.starts_with("https://")
        || candidate.starts_with("data:")
        || is_windows_path(candidate)
        || is_windows_unc_path(candidate)
}

/// Returns true for Windows-style absolute paths like `C:\…` or `D:/…`.
fn is_windows_path(candidate: &str) -> bool {
    let mut chars = candidate.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let Some(second) = chars.next() else {
        return false;
    };
    if second != ':' {
        return false;
    }
    matches!(chars.next(), Some('\\') | Some('/'))
}

fn is_windows_unc_path(candidate: &str) -> bool {
    let Some(rest) = candidate.strip_prefix(r"\\") else {
        return false;
    };
    if rest.starts_with('?') || rest.starts_with('.') {
        return false;
    }
    let mut parts = rest.splitn(2, ['\\', '/']);
    let server = parts.next().unwrap_or("");
    let share = parts.next().unwrap_or("");
    !server.is_empty() && !share.is_empty()
}

fn collapse_wrapped_marker(raw: &str) -> String {
    if !raw.contains('\n') && !raw.contains('\r') {
        return raw.trim().to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut skip_ws = false;
    for ch in raw.chars() {
        if ch == '\n' || ch == '\r' {
            skip_ws = true;
            continue;
        }
        if skip_ws {
            if ch.is_whitespace() {
                continue;
            }
            skip_ws = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// True when `content` holds an image marker, terminated or not.
///
/// This is how a provider adapter tells *residue of this crate's own marker
/// normalization* from a data URI the author wrote deliberately. An
/// unterminated marker is copied through by [`parse_image_markers`] verbatim,
/// prefix included, so the prefix is present in both the input and the cleaned
/// output whenever residue is possible.
pub(crate) fn carries_image_marker(content: &str) -> bool {
    content.contains(IMAGE_MARKER_PREFIX)
}

pub fn parse_image_markers(content: &str) -> (String, Vec<String>) {
    let mut refs = Vec::new();
    let mut cleaned = String::with_capacity(content.len());
    let mut cursor = 0usize;

    while let Some(rel_start) = content[cursor..].find(IMAGE_MARKER_PREFIX) {
        let start = cursor + rel_start;
        cleaned.push_str(&content[cursor..start]);

        let marker_start = start + IMAGE_MARKER_PREFIX.len();
        let Some(rel_end) = content[marker_start..].find(']') else {
            cleaned.push_str(&content[start..]);
            cursor = content.len();
            break;
        };

        let end = marker_start + rel_end;
        let candidate = collapse_wrapped_marker(&content[marker_start..end]);

        if candidate.is_empty() || !is_loadable_image_reference(&candidate) {
            // Preserve the original marker text (placeholders like
            // `[IMAGE:...]` or `[IMAGE:<path>]` should survive as prose
            // rather than triggering a loader error).
            cleaned.push_str(&content[start..=end]);
        } else {
            refs.push(candidate);
        }

        cursor = end + 1;
    }

    if cursor < content.len() {
        cleaned.push_str(&content[cursor..]);
    }

    (cleaned.trim().to_string(), refs)
}

pub fn count_image_markers(messages: &[ChatMessage]) -> usize {
    let latest_tool_indices = latest_tool_result_indices(messages);
    count_image_markers_with_latest_tool_results(messages, &latest_tool_indices)
}

fn count_image_markers_with_latest_tool_results(
    messages: &[ChatMessage],
    latest_tool_result_indices: &HashSet<usize>,
) -> usize {
    messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            should_normalize_message_images(*index, message, latest_tool_result_indices)
        })
        .map(|(_, message)| parse_image_markers(&message.content).1.len())
        .sum()
}

pub fn contains_image_markers(messages: &[ChatMessage]) -> bool {
    count_image_markers(messages) > 0
}

pub fn count_user_image_markers(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|message| message.role == "user" && !is_prompt_tool_result_message(message))
        .map(|message| parse_image_markers(&message.content).1.len())
        .sum()
}

pub fn count_latest_user_image_markers(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "user" && !is_prompt_tool_result_message(message))
        .map(|message| parse_image_markers(&message.content).1.len())
        .unwrap_or(0)
}

/// Media-marker kinds this module recognizes. `IMAGE` is the only kind
/// resolved into provider content parts; [`AUDIO_MARKER_KINDS`] is the strict
/// subset degraded when a loadable payload would otherwise reach the model as
/// literal text. Both marker regexes below derive their kind alternation from
/// these consts so the strip-all and strip-audio paths cannot drift apart on
/// which kinds exist. The channel grammar (`ATTACHMENT_KINDS` in
/// `crates/zeroclaw-channels/src/util.rs`) recognizes these same kinds plus
/// `LOCATION`, which carries coordinates rather than a file reference and has
/// no provider-side handling; the two lists live in different crates
/// deliberately (providers cannot depend on channels).
const MEDIA_MARKER_KINDS: &[&str] = &[
    "IMAGE", "PHOTO", "DOCUMENT", "FILE", "VIDEO", "VOICE", "AUDIO",
];

/// Marker kinds whose loadable payload must not stay model-visible. No
/// provider resolves audio into content parts, and an audio path is not
/// otherwise actionable by the model: asked what it hears, a model handed a
/// bare path tends to fabricate having played the file. Every other kind in
/// [`MEDIA_MARKER_KINDS`] keeps its payload — `IMAGE` is resolved for vision
/// downstream, and `PHOTO`/`DOCUMENT`/`FILE`/`VIDEO` paths stay actionable
/// (file tools read them, and the channel delivery contract has the model
/// copy them into outbound reply markers), so stripping those would break
/// document and file delivery.
const AUDIO_MARKER_KINDS: &[&str] = &["VOICE", "AUDIO"];

pub fn strip_media_markers(text: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(&format!(
            r"(?i)\[(?:{}):[^\]]*\]",
            MEDIA_MARKER_KINDS.join("|")
        ))
        .unwrap()
    });
    RE.replace_all(text, "[media attachment]").into_owned()
}

/// Matches the audio-kind markers ([`AUDIO_MARKER_KINDS`]), capturing the
/// payload for the loadable-reference check.
static AUDIO_MARKER_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(&format!(
        r"(?i)\[(?:{}):([^\]]*)\]",
        AUDIO_MARKER_KINDS.join("|")
    ))
    .unwrap()
});

/// Replace audio markers (`[AUDIO:...]`, `[VOICE:...]`) whose payload is a
/// *loadable* reference (absolute path, `http(s)://` URL, or `data:` URI) with
/// the same `[media attachment]` placeholder the degrade path uses, returning
/// the rewritten text and the number of markers replaced.
///
/// Non-loadable payloads are left as literal text — placeholders (`[AUDIO:...]`),
/// prose (`[AUDIO:<clip>]`), and the no-transcription note (`[Audio: attached]`)
/// are harmless and must survive — mirroring how [`parse_image_markers`]
/// preserves non-loadable `[IMAGE:...]` markers. Runs over the raw string so it
/// also cleans a marker embedded in a native tool-result JSON blob
/// (`{"content":"…[AUDIO:/clip.wav]…"}`): `[media attachment]` contains no
/// JSON-special characters, so the surrounding object stays valid.
fn strip_unplayable_audio_markers(text: &str) -> (String, usize) {
    let mut stripped = 0usize;
    let out = AUDIO_MARKER_RE.replace_all(text, |caps: &regex::Captures<'_>| {
        let payload = collapse_wrapped_marker(&caps[1]);
        if !payload.is_empty() && is_loadable_image_reference(&payload) {
            stripped += 1;
            "[media attachment]".to_string()
        } else {
            // Preserve placeholder/prose markers verbatim.
            caps[0].to_string()
        }
    });
    (out.into_owned(), stripped)
}

/// Strip loadable audio markers (see `strip_unplayable_audio_markers`)
/// across every message in `messages`, logging one degradation warning when
/// any are removed. Returns the input borrowed when no candidate marker is
/// present (the common, allocation-free path) or an owned rebuilt vector
/// otherwise.
///
/// This is the shared seam keeping a raw audio path out of provider payloads,
/// whichever route the history takes:
/// - the main iteration prep ([`prepare_messages_for_provider`], via
///   `prepare_messages_inner`), and
/// - one-shot queries that dispatch history directly without full prep (the
///   max-iteration graceful summary and the other `run_model_query` callers).
///
/// Non-audio media markers pass through untouched; see `AUDIO_MARKER_KINDS`
/// for why the split falls where it does.
pub fn sanitize_audio_markers(messages: &[ChatMessage]) -> Cow<'_, [ChatMessage]> {
    if !messages
        .iter()
        .any(|m| AUDIO_MARKER_RE.is_match(&m.content))
    {
        return Cow::Borrowed(messages);
    }

    let mut stripped = 0usize;
    let rebuilt: Vec<ChatMessage> = messages
        .iter()
        .map(|m| {
            let (content, n) = strip_unplayable_audio_markers(&m.content);
            stripped += n;
            ChatMessage {
                role: m.role.clone(),
                content,
            }
        })
        .collect();

    if stripped > 0 {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "markers_stripped": stripped,
                })),
            "multimodal: stripped unplayable audio marker(s) (AUDIO/VOICE); no provider resolves audio into content parts, so a raw path/URL was replaced with a placeholder instead of being sent to the model as text"
        );
    }

    Cow::Owned(rebuilt)
}

pub fn extract_ollama_image_payload(image_ref: &str) -> Option<String> {
    if image_ref.starts_with("data:") {
        let comma_idx = image_ref.find(',')?;
        let (_, payload) = image_ref.split_at(comma_idx + 1);
        let payload = payload.trim();
        if payload.is_empty() {
            None
        } else {
            Some(payload.to_string())
        }
    } else {
        Some(image_ref.trim().to_string()).filter(|value| !value.is_empty())
    }
}

pub(crate) fn is_prompt_tool_result_message(message: &ChatMessage) -> bool {
    message.role == "user" && message.content.trim_start().starts_with("[Tool results]")
}

fn is_tool_result_carrier(message: &ChatMessage) -> bool {
    message.role == "tool" || is_prompt_tool_result_message(message)
}

fn latest_tool_result_indices(messages: &[ChatMessage]) -> HashSet<usize> {
    let mut indices = HashSet::new();
    let Some((last_index, last_message)) = messages.iter().enumerate().next_back() else {
        return indices;
    };

    if is_prompt_tool_result_message(last_message) {
        indices.insert(last_index);
        return indices;
    }

    if last_message.role == "tool" {
        for (index, message) in messages.iter().enumerate().rev() {
            if message.role != "tool" {
                break;
            }
            indices.insert(index);
        }
    }

    indices
}

fn should_normalize_message_images(
    index: usize,
    message: &ChatMessage,
    latest_tool_result_indices: &HashSet<usize>,
) -> bool {
    if is_tool_result_carrier(message) {
        return latest_tool_result_indices.contains(&index);
    }

    message.role == "user"
}

fn stripped_image_marker_text(content: &str) -> String {
    let (cleaned, refs) = parse_image_markers(content);
    if refs.is_empty() {
        return content.to_string();
    }

    if cleaned.trim().is_empty() {
        "[image removed from history]".to_string()
    } else {
        cleaned
    }
}

fn strip_tool_result_image_markers(message: &ChatMessage) -> ChatMessage {
    if !message.content.contains(IMAGE_MARKER_PREFIX) {
        return message.clone();
    }

    if message.role == "tool"
        && let Ok(serde_json::Value::Object(mut obj)) =
            serde_json::from_str::<serde_json::Value>(&message.content)
        && let Some(serde_json::Value::String(inner)) = obj.get("content").cloned()
    {
        let stripped = stripped_image_marker_text(&inner);
        if stripped == inner {
            return message.clone();
        }

        obj.insert("content".to_string(), serde_json::Value::String(stripped));
        return ChatMessage {
            role: message.role.clone(),
            content: serde_json::Value::Object(obj).to_string(),
        };
    }

    ChatMessage {
        role: message.role.clone(),
        content: stripped_image_marker_text(&message.content),
    }
}

fn replay_message_without_stale_tool_images(
    index: usize,
    message: &ChatMessage,
    latest_tool_result_indices: &HashSet<usize>,
) -> ChatMessage {
    if is_tool_result_carrier(message) && !latest_tool_result_indices.contains(&index) {
        strip_tool_result_image_markers(message)
    } else {
        message.clone()
    }
}

async fn normalize_native_tool_result_json(
    content: &str,
    config: &MultimodalConfig,
    max_bytes: usize,
    remote_client: &Client,
    ctx: &ImageNormalizeCtx<'_>,
    cache: Option<&mut LocalImageCache>,
) -> Option<(String, bool)> {
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_str::<serde_json::Value>(content)
    else {
        return None;
    };

    let Some(serde_json::Value::String(inner)) = obj.get("content").cloned() else {
        return None;
    };

    let (cleaned_text, refs) = parse_image_markers(&inner);
    if refs.is_empty() {
        return None;
    }

    let normalized =
        normalize_image_references(&refs, config, max_bytes, remote_client, ctx, cache).await;
    let new_inner = compose_multimodal_content(
        &cleaned_text,
        &normalized.data_uris,
        normalized.skipped_count,
        refs.len(),
    );
    obj.insert("content".to_string(), serde_json::Value::String(new_inner));

    Some((
        serde_json::Value::Object(obj).to_string(),
        !normalized.data_uris.is_empty(),
    ))
}

pub async fn prepare_messages_for_provider(
    messages: &[ChatMessage],
    config: &MultimodalConfig,
) -> anyhow::Result<PreparedMessages> {
    prepare_messages_inner(messages, config, None).await
}

/// Like [`prepare_messages_for_provider`] but reuses a [`LocalImageCache`]
/// across calls so each unique local image file is read from disk at most
/// once per session (or once per modification for mutable files).
pub async fn prepare_messages_for_provider_cached(
    messages: &[ChatMessage],
    config: &MultimodalConfig,
    cache: &mut LocalImageCache,
) -> anyhow::Result<PreparedMessages> {
    prepare_messages_inner(messages, config, Some(cache)).await
}

async fn prepare_messages_inner(
    messages: &[ChatMessage],
    config: &MultimodalConfig,
    mut cache: Option<&mut LocalImageCache>,
) -> anyhow::Result<PreparedMessages> {
    // Strip loadable audio markers before any provider sees the history. Left
    // in place, an audio path reaches the model as literal text and fails
    // silently — the model typically hallucinates having played the file,
    // which is worse than an explicit degradation. `[IMAGE:...]` markers are
    // handled by the normalization below; other media kinds keep their
    // payloads for delivery. The shared seam borrows the input untouched when
    // no audio marker is present, so the common hot path stays allocation-free.
    let sanitized = sanitize_audio_markers(messages);
    let messages: &[ChatMessage] = &sanitized;

    let (max_images, max_image_size_mb) = config.effective_limits();
    let max_bytes = max_image_size_mb.saturating_mul(1024 * 1024);

    let latest_tool_indices = latest_tool_result_indices(messages);
    let total_images = count_image_markers_with_latest_tool_results(messages, &latest_tool_indices);

    if total_images == 0 {
        return Ok(PreparedMessages {
            messages: messages
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    replay_message_without_stale_tool_images(index, message, &latest_tool_indices)
                })
                .collect(),
            contains_images: false,
        });
    }

    // Normalize every image marker first, then enforce the per-request image
    // cap further below based only on images that *successfully* normalize.
    // Trimming the oldest images *before* normalization is unsafe: a newer
    // image ref that fails to load would evict an older valid one that could
    // still have been sent (see `skipped_images_do_not_consume_image_budget`).
    // The post-normalization cap keeps the most recent successful images and
    // prevents conversations from sticking once the cumulative count crosses
    // the threshold, so no pre-normalization trim is needed here.
    let remote_client = build_runtime_proxy_client_with_timeouts("model_provider.ollama", 30, 10);
    let latest_tool_indices = latest_tool_result_indices(messages);

    let mut normalized_messages = Vec::with_capacity(messages.len());
    let mut has_successful_images = false;
    for (index, message) in messages.iter().enumerate() {
        if !should_normalize_message_images(index, message, &latest_tool_indices) {
            normalized_messages.push(replay_message_without_stale_tool_images(
                index,
                message,
                &latest_tool_indices,
            ));
            continue;
        }

        if message.role == "tool"
            && let Some((prepared, contains_images)) = normalize_native_tool_result_json(
                &message.content,
                config,
                max_bytes,
                &remote_client,
                &ImageNormalizeCtx {
                    message_index: index,
                    role: &message.role,
                },
                cache.as_deref_mut(),
            )
            .await
        {
            normalized_messages.push(ChatMessage {
                role: message.role.clone(),
                content: prepared,
            });
            has_successful_images |= contains_images;
            continue;
        }

        let (cleaned_text, refs) = parse_image_markers(&message.content);
        if refs.is_empty() {
            normalized_messages.push(message.clone());
            continue;
        }

        let normalized = normalize_image_references(
            &refs,
            config,
            max_bytes,
            &remote_client,
            &ImageNormalizeCtx {
                message_index: index,
                role: &message.role,
            },
            cache.as_deref_mut(),
        )
        .await;
        let content = compose_multimodal_content(
            &cleaned_text,
            &normalized.data_uris,
            normalized.skipped_count,
            refs.len(),
        );
        has_successful_images |= !normalized.data_uris.is_empty();
        normalized_messages.push(ChatMessage {
            role: message.role.clone(),
            content,
        });
    }

    // Apply age-based trimming when configured: strip images from user messages
    // older than `max_image_turns` turns back from the end of history.
    // `max_image_turns == 0` means disabled — no age trimming.
    let age_trimmed = if config.max_image_turns > 0 {
        let before = count_image_markers(&normalized_messages);
        let trimmed = trim_images_by_age(&normalized_messages, config.max_image_turns);
        let after = count_image_markers(&trimmed);
        if after < before {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "max_image_turns": config.max_image_turns,
                        "images_before": before,
                        "images_after": after,
                        "images_dropped": before - after,
                    })),
                "multimodal: age-trimmed old images from conversation history"
            );
        }
        trimmed
    } else {
        normalized_messages
    };

    // Apply the per-request image cap after normalization so failed image refs
    // do not consume budget and evict older images that could still be sent.
    let capped_messages = if has_successful_images && count_image_markers(&age_trimmed) > max_images
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "images_after_normalization": count_image_markers(&age_trimmed),
                    "max_images": max_images,
                })),
            "multimodal: post-normalization image cap exceeded — trimming oldest images"
        );
        trim_old_images(&age_trimmed, max_images)
    } else {
        age_trimmed
    };

    Ok(PreparedMessages {
        contains_images: count_image_markers(&capped_messages) > 0,
        messages: capped_messages,
    })
}
fn trim_images_by_age(messages: &[ChatMessage], max_turns: usize) -> Vec<ChatMessage> {
    // Count user messages from the end to find the cutoff index.
    let mut user_turn_count = 0usize;
    let mut cutoff = 0usize; // messages at index < cutoff are "too old"
    for (i, m) in messages.iter().enumerate().rev() {
        if m.role == "user" {
            user_turn_count += 1;
            if user_turn_count > max_turns {
                // Everything up to and including this index is too old.
                cutoff = i + 1;
                break;
            }
        }
    }

    if cutoff == 0 {
        return messages.to_vec();
    }

    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i < cutoff && m.role == "user" {
                let (cleaned, refs) = parse_image_markers(&m.content);
                if refs.is_empty() {
                    return m.clone();
                }
                let text = if cleaned.trim().is_empty() {
                    "[image removed from history]".to_string()
                } else {
                    cleaned
                };
                ChatMessage {
                    role: m.role.clone(),
                    content: text,
                }
            } else {
                m.clone()
            }
        })
        .collect()
}

/// Strip image markers from older messages (oldest first) until the total image
/// count is within `max_images`. Keeps the text content of each message.
///
/// Eviction is per image, not per message: exactly `total - max_images` images
/// are dropped, so a message holding more images than the budget allows keeps
/// its newest ones instead of losing all of them.
fn trim_old_images(messages: &[ChatMessage], max_images: usize) -> Vec<ChatMessage> {
    let latest_tool_indices = latest_tool_result_indices(messages);
    // Find which messages (by index) contain images, oldest first.
    let image_positions: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            should_normalize_message_images(*index, message, &latest_tool_indices)
        })
        .filter_map(|(i, m)| {
            let count = parse_image_markers(&m.content).1.len();
            if count > 0 { Some((i, count)) } else { None }
        })
        .collect();

    // Determine how many images to drop (from the oldest messages).
    let total: usize = image_positions.iter().map(|(_, c)| c).sum();
    let mut to_drop = total.saturating_sub(max_images);

    // Record how many images to drop per message, oldest first. A message is
    // only partially trimmed when it holds more images than remain to drop:
    // marking the whole message would evict images the budget still allows and
    // leave the request under `max_images` (a single message holding more than
    // `max_images` would otherwise lose all of them).
    let mut drop_counts = std::collections::HashMap::new();
    for &(idx, count) in &image_positions {
        if to_drop == 0 {
            break;
        }
        let drop_here = to_drop.min(count);
        drop_counts.insert(idx, drop_here);
        to_drop -= drop_here;
    }

    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let Some(&drop_here) = drop_counts.get(&i) else {
                return replay_message_without_stale_tool_images(i, m, &latest_tool_indices);
            };

            trim_message_images(m, drop_here)
        })
        .collect()
}

/// Drop the `drop_here` oldest image markers from `text`, keeping the newest.
fn trim_image_markers(text: &str, drop_here: usize) -> String {
    let (cleaned, refs) = parse_image_markers(text);
    // Newest images within the message survive, matching the oldest-first
    // eviction order across messages.
    let retained = refs.get(drop_here..).unwrap_or(&[]);
    if retained.is_empty() {
        if cleaned.trim().is_empty() {
            "[image removed from history]".to_string()
        } else {
            cleaned
        }
    } else {
        compose_multimodal_message(&cleaned, retained)
    }
}

/// Apply [`trim_image_markers`] to a message, keeping a native tool-result JSON
/// envelope intact.
///
/// A `role = "tool"` message may carry a serialized `{"tool_call_id": ..,
/// "content": ..}` object. Trimming the serialized form would strip markers out
/// of the JSON *and* append the retained ones after the closing brace, leaving
/// text that no longer parses — the provider serializers then lose
/// `tool_call_id` and cannot emit a native tool result. Unwrap first, trim the
/// inner `content`, and re-serialize with the rest of the envelope untouched,
/// mirroring [`strip_tool_result_image_markers`] and
/// [`normalize_native_tool_result_json`].
fn trim_message_images(message: &ChatMessage, drop_here: usize) -> ChatMessage {
    if message.role == "tool"
        && let Ok(serde_json::Value::Object(mut obj)) =
            serde_json::from_str::<serde_json::Value>(&message.content)
        && let Some(serde_json::Value::String(inner)) = obj.get("content").cloned()
    {
        let trimmed = trim_image_markers(&inner, drop_here);
        obj.insert("content".to_string(), serde_json::Value::String(trimmed));
        return ChatMessage {
            role: message.role.clone(),
            content: serde_json::Value::Object(obj).to_string(),
        };
    }

    ChatMessage {
        role: message.role.clone(),
        content: trim_image_markers(&message.content, drop_here),
    }
}

fn compose_multimodal_message(text: &str, data_uris: &[String]) -> String {
    let mut content = String::new();
    let trimmed = text.trim();

    if !trimmed.is_empty() {
        content.push_str(trimmed);
        content.push_str("\n\n");
    }

    for (index, data_uri) in data_uris.iter().enumerate() {
        if index > 0 {
            content.push('\n');
        }
        content.push_str(IMAGE_MARKER_PREFIX);
        content.push_str(data_uri);
        content.push(']');
    }

    content
}

struct NormalizedImageReferences {
    data_uris: Vec<String>,
    skipped_count: usize,
}

/// Context attached to image-skip log events so callers can be identified.
struct ImageNormalizeCtx<'a> {
    /// Zero-based index of this message in the conversation history.
    message_index: usize,
    /// Role of the message containing the image reference.
    role: &'a str,
}

async fn normalize_image_references(
    refs: &[String],
    config: &MultimodalConfig,
    max_bytes: usize,
    remote_client: &Client,
    ctx: &ImageNormalizeCtx<'_>,
    mut cache: Option<&mut LocalImageCache>,
) -> NormalizedImageReferences {
    let mut data_uris = Vec::with_capacity(refs.len());
    let mut skipped_count = 0usize;

    for reference in refs {
        match normalize_image_reference(
            reference,
            config,
            max_bytes,
            remote_client,
            cache.as_deref_mut(),
        )
        .await
        {
            Ok(data_uri) => data_uris.push(data_uri),
            Err(error) => {
                skipped_count += 1;
                let error_reason = multimodal_error_reason(&error);
                // Truncate the raw reference so we don't dump a full base64
                // payload into the log, but keep enough to identify the source.
                let marker_preview: String = reference.chars().take(120).collect();
                let error_kind = multimodal_error_kind(&error);
                let attrs = ::serde_json::json!({
                    "message_index": ctx.message_index,
                    "message_role": ctx.role,
                    "source_kind": image_reference_kind(reference),
                    "error_kind": error_kind,
                    "reason": error_reason.as_deref().unwrap_or(""),
                    "marker_preview": marker_preview,
                });
                let is_tool_role = ctx.role == "tool";
                let is_recoverable_load_failure = matches!(
                    error_kind,
                    "image_source_not_found"
                        | "local_read_failed"
                        | "remote_fetch_failed"
                        | "invalid_marker"
                );
                if is_tool_role && is_recoverable_load_failure {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(attrs),
                        "skipping multimodal marker in tool result (likely not a real attachment)"
                    );
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(attrs),
                        "skipping multimodal image that could not be loaded"
                    );
                }
            }
        }
    }

    NormalizedImageReferences {
        data_uris,
        skipped_count,
    }
}

fn compose_multimodal_content(
    text: &str,
    data_uris: &[String],
    skipped_count: usize,
    total_refs: usize,
) -> String {
    if skipped_count == 0 {
        return compose_multimodal_message(text, data_uris);
    }

    let text_with_note = append_skipped_image_note(text, skipped_count, total_refs);
    if data_uris.is_empty() {
        text_with_note.trim().to_string()
    } else {
        compose_multimodal_message(&text_with_note, data_uris)
    }
}

fn append_skipped_image_note(text: &str, skipped_count: usize, total_refs: usize) -> String {
    if skipped_count == 0 {
        return text.to_string();
    }

    // This note is model-facing provider context, not direct localized UI text.
    let note = if skipped_count == total_refs {
        format!("{skipped_count} attached image(s) could not be loaded")
    } else {
        format!("{skipped_count} of {total_refs} attached image(s) could not be loaded")
    };

    let trimmed = text.trim();
    if trimmed.is_empty() {
        format!("Note: {note}.")
    } else {
        format!("{trimmed}\n\nNote: {note}.")
    }
}

fn image_reference_kind(reference: &str) -> &'static str {
    if reference.starts_with("data:") {
        "data"
    } else if reference.starts_with("http://") || reference.starts_with("https://") {
        "remote"
    } else {
        "local"
    }
}

fn multimodal_error_kind(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<MultimodalError>() {
        Some(MultimodalError::TooManyImages { .. }) => "too_many_images",
        Some(MultimodalError::ImageTooLarge { .. }) => "image_too_large",
        Some(MultimodalError::UnsupportedMime { .. }) => "unsupported_mime",
        Some(MultimodalError::RemoteFetchDisabled { .. }) => "remote_fetch_disabled",
        Some(MultimodalError::ImageSourceNotFound { .. }) => "image_source_not_found",
        Some(MultimodalError::InvalidMarker { .. }) => "invalid_marker",
        Some(MultimodalError::RemoteFetchFailed { .. }) => "remote_fetch_failed",
        Some(MultimodalError::LocalReadFailed { .. }) => "local_read_failed",
        None => "unknown",
    }
}

fn multimodal_error_reason(error: &anyhow::Error) -> Option<String> {
    match error.downcast_ref::<MultimodalError>() {
        Some(MultimodalError::InvalidMarker { input, reason })
        | Some(MultimodalError::RemoteFetchFailed { input, reason })
        | Some(MultimodalError::LocalReadFailed { input, reason }) => {
            Some(reason.replace(input, "<source>"))
        }
        _ => None,
    }
}

async fn normalize_image_reference(
    source: &str,
    config: &MultimodalConfig,
    max_bytes: usize,
    remote_client: &Client,
    cache: Option<&mut LocalImageCache>,
) -> anyhow::Result<String> {
    if source.starts_with("data:") {
        return normalize_data_uri(source, max_bytes);
    }

    if source.starts_with("http://") || source.starts_with("https://") {
        if !config.allow_remote_fetch {
            return Err(MultimodalError::RemoteFetchDisabled {
                input: source.to_string(),
            }
            .into());
        }

        return normalize_remote_image(source, max_bytes, remote_client).await;
    }

    match cache {
        Some(c) => normalize_local_image_cached(source, max_bytes, c).await,
        None => normalize_local_image(source, max_bytes).await,
    }
}

fn normalize_data_uri(source: &str, max_bytes: usize) -> anyhow::Result<String> {
    let Some(comma_idx) = source.find(',') else {
        return Err(MultimodalError::InvalidMarker {
            input: source.to_string(),
            reason: "expected data URI payload".to_string(),
        }
        .into());
    };

    let header = &source[..comma_idx];
    let payload = source[comma_idx + 1..].trim();

    if !header.contains(";base64") {
        return Err(MultimodalError::InvalidMarker {
            input: source.to_string(),
            reason: "only base64 data URIs are supported".to_string(),
        }
        .into());
    }

    let mime = header
        .trim_start_matches("data:")
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    validate_mime(source, &mime)?;

    let decoded = STANDARD
        .decode(payload)
        .map_err(|error| MultimodalError::InvalidMarker {
            input: source.to_string(),
            reason: format!("invalid base64 payload: {error}"),
        })?;

    validate_size(source, decoded.len(), max_bytes)?;

    Ok(format!("data:{mime};base64,{}", STANDARD.encode(decoded)))
}

async fn normalize_remote_image(
    source: &str,
    max_bytes: usize,
    remote_client: &Client,
) -> anyhow::Result<String> {
    let response = remote_client.get(source).send().await.map_err(|error| {
        MultimodalError::RemoteFetchFailed {
            input: source.to_string(),
            reason: error.to_string(),
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(MultimodalError::RemoteFetchFailed {
            input: source.to_string(),
            reason: format!("HTTP {status}"),
        }
        .into());
    }

    if let Some(content_length) = response.content_length() {
        let content_length = usize::try_from(content_length).unwrap_or(usize::MAX);
        validate_size(source, content_length, max_bytes)?;
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);

    let bytes = response
        .bytes()
        .await
        .map_err(|error| MultimodalError::RemoteFetchFailed {
            input: source.to_string(),
            reason: error.to_string(),
        })?;

    validate_size(source, bytes.len(), max_bytes)?;

    let mime = detect_mime(None, bytes.as_ref(), content_type.as_deref()).ok_or_else(|| {
        MultimodalError::UnsupportedMime {
            input: source.to_string(),
            mime: "unknown".to_string(),
        }
    })?;

    validate_mime(source, &mime)?;

    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

async fn normalize_local_image(source: &str, max_bytes: usize) -> anyhow::Result<String> {
    let path = Path::new(source);
    if !path.exists() || !path.is_file() {
        return Err(MultimodalError::ImageSourceNotFound {
            input: source.to_string(),
        }
        .into());
    }

    let metadata =
        tokio::fs::metadata(path)
            .await
            .map_err(|error| MultimodalError::LocalReadFailed {
                input: source.to_string(),
                reason: error.to_string(),
            })?;

    validate_size(
        source,
        usize::try_from(metadata.len()).unwrap_or(usize::MAX),
        max_bytes,
    )?;

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| MultimodalError::LocalReadFailed {
            input: source.to_string(),
            reason: error.to_string(),
        })?;

    validate_size(source, bytes.len(), max_bytes)?;

    let mime =
        detect_mime(Some(path), &bytes, None).ok_or_else(|| MultimodalError::UnsupportedMime {
            input: source.to_string(),
            mime: "unknown".to_string(),
        })?;

    validate_mime(source, &mime)?;

    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

/// Cache-aware local image loader. On a hit (path + metadata unchanged) returns
/// the stored data URI without touching the filesystem. Files under `/uploads/`
/// are content-addressed and treated as immutable — checked once, never re-read.
async fn normalize_local_image_cached(
    source: &str,
    max_bytes: usize,
    cache: &mut LocalImageCache,
) -> anyhow::Result<String> {
    let path = Path::new(source);
    if !path.exists() || !path.is_file() {
        return Err(MultimodalError::ImageSourceNotFound {
            input: source.to_string(),
        }
        .into());
    }

    let metadata =
        tokio::fs::metadata(path)
            .await
            .map_err(|error| MultimodalError::LocalReadFailed {
                input: source.to_string(),
                reason: error.to_string(),
            })?;

    let file_len = metadata.len();
    let is_immutable = source.contains("/uploads/");
    let mtime: i64 = if is_immutable {
        0
    } else {
        metadata
            .modified()
            .ok()
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64)
            })
            .unwrap_or(0)
    };
    let cache_len = if is_immutable { 0 } else { file_len };

    if let Some(cached) = cache.get(source, cache_len, mtime) {
        return Ok(cached.to_string());
    }

    validate_size(
        source,
        usize::try_from(file_len).unwrap_or(usize::MAX),
        max_bytes,
    )?;

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| MultimodalError::LocalReadFailed {
            input: source.to_string(),
            reason: error.to_string(),
        })?;

    validate_size(source, bytes.len(), max_bytes)?;

    let mime =
        detect_mime(Some(path), &bytes, None).ok_or_else(|| MultimodalError::UnsupportedMime {
            input: source.to_string(),
            mime: "unknown".to_string(),
        })?;

    validate_mime(source, &mime)?;

    let data_uri = format!("data:{mime};base64,{}", STANDARD.encode(&bytes));
    cache.insert(source.to_string(), cache_len, mtime, data_uri.clone());
    Ok(data_uri)
}

fn validate_size(source: &str, size_bytes: usize, max_bytes: usize) -> anyhow::Result<()> {
    if size_bytes > max_bytes {
        return Err(MultimodalError::ImageTooLarge {
            input: source.to_string(),
            size_bytes,
            max_bytes,
        }
        .into());
    }

    Ok(())
}

fn validate_mime(source: &str, mime: &str) -> anyhow::Result<()> {
    if ALLOWED_IMAGE_MIME_TYPES.contains(&mime) {
        return Ok(());
    }

    Err(MultimodalError::UnsupportedMime {
        input: source.to_string(),
        mime: mime.to_string(),
    }
    .into())
}

fn detect_mime(
    path: Option<&Path>,
    bytes: &[u8],
    header_content_type: Option<&str>,
) -> Option<String> {
    if let Some(header_mime) = header_content_type.and_then(normalize_content_type) {
        return Some(header_mime);
    }

    if let Some(path) = path
        && let Some(ext) = path.extension().and_then(|value| value.to_str())
        && let Some(mime) = mime_from_extension(ext)
    {
        return Some(mime.to_string());
    }

    mime_from_magic(bytes).map(ToString::to_string)
}

fn normalize_content_type(content_type: &str) -> Option<String> {
    let mime = content_type.split(';').next()?.trim().to_ascii_lowercase();
    if mime.is_empty() { None } else { Some(mime) }
}

fn mime_from_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn mime_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Some("image/png");
    }

    if bytes.len() >= 3 && bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }

    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }

    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }

    if bytes.len() >= 2 && bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }

    None
}

#[cfg(test)]
mod tests;
