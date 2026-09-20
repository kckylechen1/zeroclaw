use anyhow::Context;
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use reqwest::multipart::{Form, Part};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_config::schema::{Config, StreamMode, TELEGRAM_OFFICIAL_API_BASE_URL};
use zeroclaw_runtime::security::pairing::PairingGuard;

/// Telegram's maximum message length for text messages
const TELEGRAM_MAX_MESSAGE_LENGTH: usize = 4096;
const TELEGRAM_CONTINUED_PREFIX: &str = "(continued)\n\n";
const TELEGRAM_CONTINUES_SUFFIX: &str = "\n\n(continues...)";
const TELEGRAM_FENCE_REOPEN: &str = "```\n";
const TELEGRAM_FENCE_CLOSE: &str = "```";
const TELEGRAM_ACK_REACTIONS: &[&str] = &["⚡️", "👌", "👀", "🔥", "👍"];

/// Operator-authored skip marker for a poisoned Telegram update: written
/// by `zeroclaw telegram skip-update`, consumed by the retry loop before
/// its next re-attempt. Explicit human action only — nothing here ever
/// drops an update automatically.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TelegramSkipMarker {
    pub update_id: i64,
    /// Operator-supplied reason, archived alongside the dead letter.
    pub reason: String,
    /// Unix epoch seconds when the CLI wrote the marker.
    pub created_at_unix: u64,
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the operator skip list for `alias` lives under the daemon's
/// `data_dir`. Shared contract between the CLI writer and the listener.
#[must_use]
pub fn telegram_skip_list_path(data_dir: &Path, alias: &str) -> std::path::PathBuf {
    data_dir.join("telegram_skip").join(format!("{alias}.json"))
}

/// Best-effort read of the skip list; a missing or malformed file means
/// "no skips" — the retry loop must never fail closed on operator file
/// trouble.
pub fn load_telegram_skip_markers(data_dir: &Path, alias: &str) -> Vec<TelegramSkipMarker> {
    let path = telegram_skip_list_path(data_dir, alias);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<Vec<TelegramSkipMarker>>(&raw).unwrap_or_else(|err| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "path": path.display().to_string(),
                        "err": err.to_string(),
                    })),
                "telegram skip list unreadable; treating as empty"
            );
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Append a skip marker (the `zeroclaw telegram skip-update` entry
/// point). Atomic tmp+rename so the listener never observes a torn file.
/// Idempotent per update_id.
pub fn append_telegram_skip_marker(
    data_dir: &Path,
    alias: &str,
    update_id: i64,
    reason: &str,
) -> Result<(), String> {
    if alias.is_empty() || alias.contains('/') || alias.contains('\\') {
        return Err(format!(
            "invalid bot alias '{alias}': path separators are not allowed"
        ));
    }
    let path = telegram_skip_list_path(data_dir, alias);
    let mut markers = load_telegram_skip_markers(data_dir, alias);
    if markers.iter().any(|marker| marker.update_id == update_id) {
        return Ok(());
    }
    markers.push(TelegramSkipMarker {
        update_id,
        reason: reason.to_string(),
        created_at_unix: unix_now_secs(),
    });
    let parent = path
        .parent()
        .ok_or_else(|| "skip list has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    let body = serde_json::to_string_pretty(&markers).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut file) = std::fs::File::create(&tmp) {
        use std::io::Write as _;
        if let Ok(()) = file.write_all(body.as_bytes()) {
            let _ = file.sync_all();
        }
    } else {
        return Err(format!("write {}: open failed", tmp.display()));
    }
    std::fs::rename(&tmp, &path).map_err(|err| format!("rename into {}: {err}", path.display()))?;
    Ok(())
}

/// Metadata for an incoming document or photo attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IncomingAttachment {
    file_id: String,
    file_name: Option<String>,
    file_size: Option<u64>,
    caption: Option<String>,
    kind: IncomingAttachmentKind,
}

/// The kind of incoming attachment (document vs photo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingAttachmentKind {
    Document,
    Photo,
}
const TELEGRAM_BIND_COMMAND: &str = "/bind";
/// Telegram Bot API allows at most 100 commands via setMyCommands.
const TELEGRAM_MAX_BOT_COMMANDS: usize = 100;
/// Telegram command names: 1-32 lowercase a-z, 0-9, and underscore.
const TELEGRAM_COMMAND_NAME_MAX_LEN: usize = 32;
/// Telegram command descriptions nominally allow up to 256 characters per the API docs,
/// but empirical testing shows the API returns errors for descriptions substantially
/// longer than 100 characters. This conservative cap avoids that in practice.
const TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN: usize = 100;

/// Sanitize a skill name into a valid Telegram command name.
/// Telegram commands must be 1-32 characters, lowercase a-z, 0-9, underscore only.
fn sanitize_telegram_command_name(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            result.push(lower);
        } else if !result.ends_with('_') {
            // Replace non-alphanumeric with underscore, collapsing consecutive runs.
            result.push('_');
        }
    }

    let trimmed = result.trim_matches('_');
    if trimmed.len() <= TELEGRAM_COMMAND_NAME_MAX_LEN {
        trimmed.to_string()
    } else {
        trimmed[..TELEGRAM_COMMAND_NAME_MAX_LEN]
            .trim_end_matches('_')
            .to_string()
    }
}

/// Truncate a description to the conservative `TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN` cap.
/// The API nominally supports 256 characters, but empirical testing shows errors occur
/// for descriptions substantially longer than 100 characters.
fn truncate_telegram_command_description(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN {
        return trimmed.to_string();
    }
    let mut truncated: String = trimmed
        .chars()
        .take(TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN - 1)
        .collect();
    truncated.push('…');
    truncated
}

/// Split a message into chunks that respect Telegram's 4096 character limit.
/// Tries to split at word boundaries when possible, and handles continuation.
/// The split budget includes continuation markers and synthetic code fences
/// exactly as `send_text_chunks` will send them.
fn split_message_for_telegram(message: &str) -> Vec<String> {
    if message.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH {
        return vec![message.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = message;
    let mut in_code_block = false;

    while !remaining.is_empty() {
        let has_previous = !chunks.is_empty();

        if telegram_chunk_send_len(remaining, in_code_block, has_previous, false)
            <= TELEGRAM_MAX_MESSAGE_LENGTH
        {
            let chunk = build_telegram_chunk(remaining, in_code_block, false);
            chunks.push(chunk);
            break;
        }

        let max_take = max_nonfinal_telegram_raw_chars(remaining, in_code_block, has_previous);
        let hard_split = byte_index_after_chars(remaining, max_take);
        let chunk_end = preferred_telegram_split_end(
            remaining,
            hard_split,
            max_take,
            in_code_block,
            has_previous,
        );

        let raw_chunk = &remaining[..chunk_end];
        let starts_in_code_block = in_code_block;
        in_code_block = code_block_state_after(raw_chunk, in_code_block);
        chunks.push(build_telegram_chunk(raw_chunk, starts_in_code_block, true));
        remaining = &remaining[chunk_end..];
    }

    chunks
}

fn build_telegram_chunk(raw_chunk: &str, starts_in_code_block: bool, has_next: bool) -> String {
    let reopen_prefix = if starts_in_code_block {
        TELEGRAM_FENCE_REOPEN
    } else {
        ""
    };
    let ends_in_code_block = code_block_state_after(raw_chunk, starts_in_code_block);
    let needs_synthetic_close = has_next && ends_in_code_block;
    let mut chunk = String::with_capacity(
        reopen_prefix.len()
            + raw_chunk.len()
            + if needs_synthetic_close {
                "\n```".len()
            } else {
                0
            },
    );
    chunk.push_str(reopen_prefix);
    chunk.push_str(raw_chunk);
    if needs_synthetic_close {
        if !chunk.ends_with('\n') {
            chunk.push('\n');
        }
        chunk.push_str(TELEGRAM_FENCE_CLOSE);
    }
    chunk
}

fn format_telegram_text_chunk(chunk: &str, index: usize, total: usize) -> String {
    if total <= 1 {
        return chunk.to_string();
    }

    if index == 0 {
        format!("{chunk}{TELEGRAM_CONTINUES_SUFFIX}")
    } else if index == total - 1 {
        format!("{TELEGRAM_CONTINUED_PREFIX}{chunk}")
    } else {
        format!("{TELEGRAM_CONTINUED_PREFIX}{chunk}{TELEGRAM_CONTINUES_SUFFIX}")
    }
}

fn telegram_chunk_marker_len(has_previous: bool, has_next: bool) -> usize {
    let prefix_len = if has_previous {
        TELEGRAM_CONTINUED_PREFIX.chars().count()
    } else {
        0
    };
    let suffix_len = if has_next {
        TELEGRAM_CONTINUES_SUFFIX.chars().count()
    } else {
        0
    };
    prefix_len + suffix_len
}

fn telegram_chunk_body_len(raw_chunk: &str, starts_in_code_block: bool, has_next: bool) -> usize {
    let reopen_len = if starts_in_code_block {
        TELEGRAM_FENCE_REOPEN.chars().count()
    } else {
        0
    };
    let raw_len = raw_chunk.chars().count();
    let ends_in_code_block = code_block_state_after(raw_chunk, starts_in_code_block);
    let synthetic_close_len = if has_next && ends_in_code_block {
        TELEGRAM_FENCE_CLOSE.chars().count() + usize::from(!raw_chunk.ends_with('\n'))
    } else {
        0
    };

    reopen_len + raw_len + synthetic_close_len
}

fn telegram_chunk_send_len(
    raw_chunk: &str,
    starts_in_code_block: bool,
    has_previous: bool,
    has_next: bool,
) -> usize {
    telegram_chunk_marker_len(has_previous, has_next)
        + telegram_chunk_body_len(raw_chunk, starts_in_code_block, has_next)
}

fn max_nonfinal_telegram_raw_chars(
    remaining: &str,
    starts_in_code_block: bool,
    has_previous: bool,
) -> usize {
    let remaining_chars = remaining.chars().count();
    let marker_len = telegram_chunk_marker_len(has_previous, true);
    let reopen_len = if starts_in_code_block {
        TELEGRAM_FENCE_REOPEN.chars().count()
    } else {
        0
    };
    let upper = remaining_chars
        .saturating_sub(1)
        .min(TELEGRAM_MAX_MESSAGE_LENGTH - marker_len - reopen_len);

    for take in (1..=upper).rev() {
        let end = byte_index_after_chars(remaining, take);
        if telegram_chunk_send_len(&remaining[..end], starts_in_code_block, has_previous, true)
            <= TELEGRAM_MAX_MESSAGE_LENGTH
        {
            return take;
        }
    }

    1
}

fn byte_index_after_chars(s: &str, char_count: usize) -> usize {
    if char_count == 0 {
        return 0;
    }
    s.char_indices()
        .nth(char_count)
        .map_or(s.len(), |(idx, _)| idx)
}

fn preferred_telegram_split_end(
    remaining: &str,
    hard_split: usize,
    max_take: usize,
    starts_in_code_block: bool,
    has_previous: bool,
) -> usize {
    let search_area = &remaining[..hard_split];
    let candidate_fits = |end: usize| {
        end > 0
            && end < remaining.len()
            && telegram_chunk_send_len(&remaining[..end], starts_in_code_block, has_previous, true)
                <= TELEGRAM_MAX_MESSAGE_LENGTH
    };

    if let Some(pos) = search_area.rfind('\n') {
        let end = pos + '\n'.len_utf8();
        if search_area[..pos].chars().count() >= max_take / 2 && candidate_fits(end) {
            return end;
        }
    }

    if let Some(pos) = search_area.rfind(' ') {
        let end = pos + ' '.len_utf8();
        if candidate_fits(end) {
            return end;
        }
    }

    hard_split
}

fn code_block_state_after(text: &str, mut in_code_block: bool) -> bool {
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
        }
    }
    in_code_block
}

fn pick_uniform_index(len: usize) -> usize {
    debug_assert!(len > 0);
    let upper = len as u64;
    let reject_threshold = (u64::MAX / upper) * upper;

    loop {
        let value = rand::random::<u64>();
        if value < reject_threshold {
            #[allow(clippy::cast_possible_truncation)]
            return (value % upper) as usize;
        }
    }
}

fn random_telegram_ack_reaction() -> &'static str {
    TELEGRAM_ACK_REACTIONS[pick_uniform_index(TELEGRAM_ACK_REACTIONS.len())]
}

fn build_telegram_ack_reaction_request(
    chat_id: &str,
    message_id: i64,
    emoji: &str,
) -> serde_json::Value {
    serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction": [{
            "type": "emoji",
            "emoji": emoji
        }]
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramAttachmentKind {
    Image,
    Document,
    Video,
    Audio,
    Voice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramAttachment {
    kind: TelegramAttachmentKind,
    target: String,
}

impl TelegramAttachmentKind {
    fn from_marker(marker: &str) -> Option<Self> {
        match marker.trim().to_ascii_uppercase().as_str() {
            "IMAGE" | "PHOTO" => Some(Self::Image),
            "DOCUMENT" | "FILE" => Some(Self::Document),
            "VIDEO" => Some(Self::Video),
            "AUDIO" => Some(Self::Audio),
            "VOICE" => Some(Self::Voice),
            _ => None,
        }
    }
}

/// Check whether a file path has a recognized image extension.
fn is_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
            )
        })
        .unwrap_or(false)
}

fn telegram_audio_send_spec(
    format: &str,
) -> anyhow::Result<(&'static str, &'static str, &'static str, &'static str)> {
    Ok(match format.trim().to_ascii_lowercase().as_str() {
        "opus" | "ogg" => ("sendVoice", "voice", "voice.ogg", "audio/ogg"),
        "mp3" | "mpeg" => ("sendAudio", "audio", "voice.mp3", "audio/mpeg"),
        "wav" => ("sendAudio", "audio", "voice.wav", "audio/wav"),
        "aac" => ("sendAudio", "audio", "voice.aac", "audio/aac"),
        "flac" => ("sendAudio", "audio", "voice.flac", "audio/flac"),
        // Raw PCM is not a container format; reject so the caller reconfigures
        // the TTS provider to emit a supported container format.
        "pcm" => {
            return Err(anyhow::Error::msg(
                "Telegram does not accept raw PCM audio; \
                 configure the TTS provider to output opus, mp3, wav, aac, or flac",
            ));
        }
        _ => (
            "sendAudio",
            "audio",
            "voice.bin",
            "application/octet-stream",
        ),
    })
}

fn format_attachment_content(
    kind: IncomingAttachmentKind,
    local_filename: &str,
    local_path: &Path,
) -> String {
    match kind {
        IncomingAttachmentKind::Photo | IncomingAttachmentKind::Document
            if is_image_extension(local_path) =>
        {
            format!("[IMAGE:{}]", local_path.display())
        }
        _ => {
            format!("[Document: {}] {}", local_filename, local_path.display())
        }
    }
}

fn is_http_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

fn infer_attachment_kind_from_target(target: &str) -> Option<TelegramAttachmentKind> {
    let normalized = target
        .split('?')
        .next()
        .unwrap_or(target)
        .split('#')
        .next()
        .unwrap_or(target);

    let extension = Path::new(normalized)
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();

    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(TelegramAttachmentKind::Image),
        "mp4" | "mov" | "mkv" | "avi" | "webm" => Some(TelegramAttachmentKind::Video),
        "mp3" | "m4a" | "wav" | "flac" => Some(TelegramAttachmentKind::Audio),
        "ogg" | "oga" | "opus" => Some(TelegramAttachmentKind::Voice),
        "pdf" | "txt" | "md" | "csv" | "json" | "zip" | "tar" | "gz" | "doc" | "docx" | "xls"
        | "xlsx" | "ppt" | "pptx" => Some(TelegramAttachmentKind::Document),
        _ => None,
    }
}

fn parse_path_only_attachment(message: &str) -> Option<TelegramAttachment> {
    let trimmed = message.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }

    let candidate = trimmed.trim_matches(|c| matches!(c, '`' | '"' | '\''));
    if candidate.chars().any(char::is_whitespace) {
        return None;
    }

    let candidate = candidate.strip_prefix("file://").unwrap_or(candidate);
    let kind = infer_attachment_kind_from_target(candidate)?;

    if !is_http_url(candidate) && !Path::new(candidate).exists() {
        return None;
    }

    Some(TelegramAttachment {
        kind,
        target: candidate.to_string(),
    })
}

/// Delegate to the shared `strip_tool_call_tags` in the orchestrator module.
fn strip_tool_call_tags(message: &str) -> String {
    crate::orchestrator::strip_tool_call_tags(message)
}

fn find_matching_close(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_attachment_markers(message: &str) -> (String, Vec<TelegramAttachment>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut attachments = Vec::new();
    let mut cursor = 0;

    while cursor < message.len() {
        let Some(open_rel) = message[cursor..].find('[') else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let open = cursor + open_rel;
        cleaned.push_str(&message[cursor..open]);

        let Some(close_rel) = find_matching_close(&message[open + 1..]) else {
            cleaned.push_str(&message[open..]);
            break;
        };

        let close = open + 1 + close_rel;
        let marker = &message[open + 1..close];

        let parsed = marker.split_once(':').and_then(|(kind, target)| {
            let kind = TelegramAttachmentKind::from_marker(kind)?;
            let target = target.trim();
            if target.is_empty() {
                return None;
            }
            Some(TelegramAttachment {
                kind,
                target: target.to_string(),
            })
        });

        if let Some(attachment) = parsed {
            attachments.push(attachment);
        } else {
            cleaned.push_str(&message[open..=close]);
        }

        cursor = close + 1;
    }

    (cleaned.trim().to_string(), attachments)
}

/// Telegram Bot API maximum file download size (20 MB).
const TELEGRAM_MAX_FILE_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

/// Default minimum interval between Telegram draft edits.
const TELEGRAM_DRAFT_UPDATE_INTERVAL_MS: u64 = 1000;

/// Telegram channel — long-polls the Bot API for updates
pub struct TelegramChannel {
    bot_token: String,
    /// The alias key under `[channels.telegram.<alias>]` this handle is
    /// bound to. Used to scope peer-group writes and resolver lookups.
    alias: String,
    /// Resolves inbound external peers from canonical state at message-time.
    /// No cache (see AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH").
    peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    persist: Option<Arc<RwLock<Config>>>,
    pairing: Option<PairingGuard>,
    client: reqwest::Client,
    typing_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stream_mode: StreamMode,
    draft_update_interval_ms: u64,
    last_draft_edit: Mutex<std::collections::HashMap<String, std::time::Instant>>,
    mention_only: bool,
    bot_username: Mutex<Option<String>>,
    bot_id: Mutex<Option<i64>>,
    /// Base URL for the Telegram Bot API. Defaults to `https://api.telegram.org`.
    /// Override for local Bot API servers or testing.
    api_base: String,
    transcription: Option<zeroclaw_config::schema::TranscriptionConfig>,
    transcription_manager: Option<std::sync::Arc<super::transcription::TranscriptionManager>>,
    voice_transcriptions: Mutex<std::collections::HashMap<String, String>>,
    workspace_dir: Option<std::path::PathBuf>,
    ack_reactions: bool,
    tts_manager: Option<Arc<super::tts::TtsManager>>,
    voice_chats: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Resolves voice peers from canonical config at call-time.
    /// See AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH" — no cache.
    voice_peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    pending_voice:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>>,
    /// Per-channel proxy URL override.
    proxy_url: Option<String>,
    /// Pre-computed tool command specs (name, description) for bot command registration.
    tool_command_specs: Vec<(String, String)>,
    /// Pending approval requests: callback_data key → oneshot sender.
    /// `listen()` resolves these when a matching `callback_query` arrives.
    pending_approvals: Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::oneshot::Sender<zeroclaw_api::channel::ChannelApprovalResponse>,
            >,
        >,
    >,
    /// Seconds to wait for the operator to tap an inline-keyboard button on a
    /// tool approval prompt before auto-denying. Configurable via
    /// `channels.telegram.approval_timeout_secs`. Default: 120.
    approval_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditMessageResult {
    Success,
    NotModified,
    Failed(reqwest::StatusCode),
}

/// Outcome of attempting to parse a single incoming Telegram update.
///
/// ⚠️ This enum is a *parser* outcome, not the acknowledgement source of
/// truth. `SkipPermanent` is overloaded: it means both "this parser does not
/// apply, try the next one" and "this update is genuinely, permanently
/// handled" (unauthorized sender, mention gate, duration/size limits, missing
/// config). Those two meanings are only safe to conflate because the parser
/// chain is mutually exclusive and attempted in a fixed order (text → voice →
/// attachment), so a `SkipPermanent` that falls out of the *last* parser is
/// always a genuine permanent skip.
///
/// Whether an update is acknowledged is therefore decided by
/// [`TelegramChannel::process_update`] together with [`UpdateOutcome`] — read
/// those two to reason about offset advancement, not this enum alone.
enum UpdateDisposition {
    // Boxed: `ChannelMessage` is far larger than the unit variants, and this
    // enum is constructed on every incoming update regardless of outcome.
    Parsed(Box<ChannelMessage>),
    SkipPermanent,
    RetryTransient,
}

/// Result of routing a single update through [`TelegramChannel::process_update`].
///
/// Both the startup/restart probe and the main long-poll loop drive their
/// batches of updates through the same per-update path so a queued update
/// seen at startup gets exactly the same offset-advance discipline as one
/// seen mid-run: the offset only moves past an update once it has been
/// delivered or permanently skipped, never while a transient failure or a
/// dropped receiver could still cause it to be lost.
enum UpdateOutcome {
    /// The update was delivered or permanently skipped; the offset has been
    /// advanced past it and the caller should keep processing the batch.
    Advanced,
    /// A transient failure occurred. The caller should stop processing the
    /// rest of this batch so the next poll retries starting at the
    /// still-unadvanced offset.
    StopBatch,
    /// The channel receiver has been dropped; the whole listen loop must
    /// exit immediately.
    ReceiverClosed,
}

/// Why a Telegram `getFile` lookup failed, classified for retry purposes.
///
/// Classification is deliberately conservative: only a confidently permanent
/// vendor rejection is `Permanent`. Permanence requires structured evidence
/// from the Bot API itself — `ok: false` *and* an `error_code` on the
/// explicit whitelist `{400, 403, 404, 410}`. The upstream typed-disposition
/// port treats the rest of 4xx (minus 408/429) as permanent; that is looser
/// than this fork and would silently drop updates on retryable codes such
/// as 425. 408, 425,
/// 429, any response carrying `parameters.retry_after`, 5xx, transport
/// errors, malformed bodies, body-less non-2xx responses, and anything
/// unrecognised stay `Transient`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileLookupFailure {
    Transient,
    Permanent,
}

/// A `getFile` failure with the vendor diagnostics preserved.
#[derive(Debug)]
pub(crate) struct FileLookupError {
    pub(crate) kind: FileLookupFailure,
    pub(crate) message: String,
}

impl std::fmt::Display for FileLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl FileLookupError {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: FileLookupFailure::Transient,
            message: message.into(),
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            kind: FileLookupFailure::Permanent,
            message: message.into(),
        }
    }

    /// Map a `getFile` response onto a classified failure.
    ///
    /// `status` is the HTTP status; `body` is the parsed JSON envelope when
    /// one could be parsed. Telegram returns errors both as non-2xx statuses
    /// and as `200 OK` with `ok: false`, so both shapes are inspected.
    pub(crate) fn classify(status: reqwest::StatusCode, body: Option<&serde_json::Value>) -> Self {
        let ok_flag = body
            .and_then(|b| b.get("ok"))
            .and_then(serde_json::Value::as_bool);
        let error_code = body
            .and_then(|b| b.get("error_code"))
            .and_then(serde_json::Value::as_i64);
        let description = body
            .and_then(|b| b.get("description"))
            .and_then(serde_json::Value::as_str);

        let detail = format!(
            "Telegram getFile failed (http {}, error_code {}, ok {}): {}",
            status.as_u16(),
            error_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            ok_flag
                .map(|b| b.to_string())
                .unwrap_or_else(|| "-".to_string()),
            description.unwrap_or("no description"),
        );

        // Explicit permanent whitelist — not "all 4xx minus a denylist".
        // 408/425/429 and any payload with `parameters.retry_after` stay
        // transient so a retryable vendor code cannot silently drop an update.
        const PERMANENT_ERROR_CODES: [i64; 4] = [400, 403, 404, 410];
        let retry_after = body
            .and_then(|b| b.get("parameters"))
            .and_then(|p| p.get("retry_after"))
            .is_some();

        let terminal_rejection = !retry_after
            && ok_flag == Some(false)
            && error_code
                .map(|c| PERMANENT_ERROR_CODES.contains(&c))
                .unwrap_or(false);

        if terminal_rejection {
            Self::permanent(detail)
        } else {
            Self::transient(detail)
        }
    }
}

fn normalize_telegram_api_base(api_base: &str) -> String {
    api_base.trim_end_matches('/').to_string()
}

impl TelegramChannel {
    pub fn new(
        bot_token: String,
        alias: impl Into<String>,
        peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        mention_only: bool,
    ) -> Self {
        let alias = alias.into();
        let has_peers = !peer_resolver().is_empty();
        let pairing = if has_peers {
            None
        } else {
            let guard = PairingGuard::new(true, &[]);
            if let Some(code) = guard.pairing_code() {
                // Surface the one-time bind code through the structured log,
                // not just stdout. A backgrounded daemon (launchd/systemd/
                // GUI-spawned) discards stdout, so the println! alone leaves
                // the operator with no way to retrieve the code. The log
                // lands in runtime-trace.jsonl, the gateway log stream, and
                // `zeroclaw service logs`. Tag it `Channel` (not the default
                // `Internal`) so it survives the web Logs page's default
                // hide-internal filter and is visible without unticking it.
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_attrs(::serde_json::json!({
                            "alias": alias.as_str(),
                            "pairing_code": code.as_str(),
                        })),
                    "Telegram pairing required; one-time bind code issued"
                );
                println!("  🔐 Telegram pairing required. One-time bind code: {code}");
                println!("     Send `{TELEGRAM_BIND_COMMAND} <code>` from your Telegram account.");
            }
            Some(guard)
        };

        Self {
            bot_token,
            alias,
            peer_resolver,
            persist: None,
            pairing,
            client: reqwest::Client::new(),
            stream_mode: StreamMode::Off,
            draft_update_interval_ms: TELEGRAM_DRAFT_UPDATE_INTERVAL_MS,
            last_draft_edit: Mutex::new(std::collections::HashMap::new()),
            typing_handle: Mutex::new(None),
            mention_only,
            bot_username: Mutex::new(None),
            bot_id: Mutex::new(None),
            api_base: TELEGRAM_OFFICIAL_API_BASE_URL.to_string(),
            transcription: None,
            transcription_manager: None,
            voice_transcriptions: Mutex::new(std::collections::HashMap::new()),
            workspace_dir: None,
            ack_reactions: true,
            tts_manager: None,
            voice_chats: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            voice_peer_resolver: Arc::new(Vec::new) as Arc<dyn Fn() -> Vec<String> + Send + Sync>,
            pending_voice: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            proxy_url: None,
            tool_command_specs: Vec::new(),
            pending_approvals: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            approval_timeout_secs: 120,
        }
    }

    /// Set the resolver used to resolve voice-chat peers live (no cached state).
    pub fn with_voice_peer_resolver(
        mut self,
        voice_peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    ) -> Self {
        self.voice_peer_resolver = voice_peer_resolver;
        self
    }

    /// Override the approval prompt timeout (default 120s).
    pub fn with_approval_timeout_secs(mut self, secs: u64) -> Self {
        self.approval_timeout_secs = secs;
        self
    }

    /// Configure whether Telegram-native acknowledgement reactions are sent.
    pub fn with_ack_reactions(mut self, enabled: bool) -> Self {
        self.ack_reactions = enabled;
        self
    }

    /// Returns `true` if `recipient` is in a peer group configured with
    /// `output_modality = "voice"` for this channel. Resolved live from config
    /// via `voice_peer_resolver` so it stays correct across hot-reloads.
    pub(crate) fn is_voice_peer(&self, recipient: &str) -> bool {
        (self.voice_peer_resolver)().iter().any(|p| p == recipient)
    }

    /// Set a per-channel proxy URL that overrides the global proxy config.
    pub fn with_proxy_url(mut self, proxy_url: Option<String>) -> Self {
        self.proxy_url = proxy_url;
        self
    }

    /// Store pre-computed tool command specs for bot command registration.
    pub fn with_tool_command_specs(mut self, specs: Vec<(String, String)>) -> Self {
        self.tool_command_specs = specs;
        self
    }

    /// Configure workspace directory for saving downloaded attachments.
    pub fn with_workspace_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(dir);
        self
    }

    /// Configure streaming mode for progressive draft updates.
    pub fn with_streaming(
        mut self,
        stream_mode: StreamMode,
        draft_update_interval_ms: u64,
    ) -> Self {
        self.stream_mode = stream_mode;
        self.draft_update_interval_ms = if draft_update_interval_ms == 0 {
            TELEGRAM_DRAFT_UPDATE_INTERVAL_MS
        } else {
            draft_update_interval_ms
        };
        self
    }

    /// Override the Telegram Bot API base URL.
    /// Useful for local Bot API servers or testing.
    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = normalize_telegram_api_base(&api_base);
        self
    }

    /// Configure voice transcription.
    pub fn with_transcription(
        mut self,
        config: zeroclaw_config::schema::TranscriptionConfig,
    ) -> Self {
        if !config.enabled {
            return self;
        }
        match super::transcription::TranscriptionManager::new(&config) {
            Ok(m) => {
                let names = m.available_providers();
                let m = if names.len() == 1 {
                    let only = names[0].to_string();
                    m.with_agent_transcription_provider(only)
                } else {
                    m
                };
                self.transcription_manager = Some(std::sync::Arc::new(m));
                self.transcription = Some(config);
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"e": e.to_string()})),
                    "transcription manager init failed, voice transcription disabled"
                );
            }
        }
        self
    }

    pub fn with_typed_transcription_providers(
        mut self,
        typed: &zeroclaw_config::providers::TranscriptionProviders,
        agent_alias: &str,
    ) -> Self {
        if agent_alias.is_empty() || typed.is_empty() {
            return self;
        }
        let base = match self.transcription_manager.take() {
            Some(arc) => match std::sync::Arc::try_unwrap(arc) {
                Ok(m) => m,
                Err(arc) => {
                    self.transcription_manager = Some(arc);
                    return self;
                }
            },
            None => super::transcription::TranscriptionManager::empty(),
        };
        let updated = base
            .with_typed_providers(typed)
            .with_agent_transcription_provider(agent_alias.to_string());
        self.transcription_manager = Some(std::sync::Arc::new(updated));
        self
    }

    /// Set the agent transcription provider alias on the internal TranscriptionManager.
    /// Must be called after `with_transcription`. No-op if transcription was not configured.
    /// The alias should be the provider type key ("groq", "openai", etc.) registered in
    /// the TranscriptionManager, or the full "type.alias" form (the type prefix is extracted).
    pub fn with_agent_transcription_provider(mut self, alias: impl Into<String>) -> Self {
        let alias = alias.into();
        if alias.is_empty() {
            return self;
        }
        // Resolve "groq.default" → "groq" (TranscriptionManager keys by type, not full alias)
        let key = alias.split('.').next().unwrap_or(&alias).to_string();
        if let Some(manager) = self.transcription_manager.take() {
            match std::sync::Arc::try_unwrap(manager) {
                Ok(m) => {
                    self.transcription_manager = Some(std::sync::Arc::new(
                        m.with_agent_transcription_provider(key),
                    ));
                }
                Err(arc) => {
                    self.transcription_manager = Some(arc);
                }
            }
        }
        self
    }

    pub fn with_tts(mut self, config: &zeroclaw_config::schema::Config) -> Self {
        if config.tts.enabled {
            let owner = config.agent_for_channel(&format!("telegram.{}", self.alias));
            match super::tts::TtsManager::from_config_for_agent(config, owner) {
                Ok(m) => self.tts_manager = Some(Arc::new(m)),
                Err(e) => ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "TTS disabled"
                ),
            }
        }
        self
    }

    /// Parse reply_target into (chat_id, optional thread_id).
    fn parse_reply_target(reply_target: &str) -> (String, Option<String>) {
        if let Some((chat_id, thread_id)) = reply_target.split_once(':') {
            (chat_id.to_string(), Some(thread_id.to_string()))
        } else {
            (reply_target.to_string(), None)
        }
    }

    fn extract_update_message_target(update: &serde_json::Value) -> Option<(String, i64)> {
        let message = update.get("message")?;
        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)?
            .to_string();
        let message_id = message
            .get("message_id")
            .and_then(serde_json::Value::as_i64)?;
        Some((chat_id, message_id))
    }

    fn try_add_ack_reaction_nonblocking(&self, chat_id: String, message_id: i64) {
        let client = self.http_client();
        let url = self.api_url("setMessageReaction");
        let emoji = random_telegram_ack_reaction().to_string();
        let body = build_telegram_ack_reaction_request(&chat_id, message_id, &emoji);

        zeroclaw_spawn::spawn!(async move {
            let response = match client.post(&url).json(&body).send().await {
                Ok(resp) => resp,
                Err(err) => {
                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"chat_id": chat_id, "message_id": message_id, "err": err.to_string()})), "failed to add ACK reaction to chat_id=, message_id=");
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let err_body = response.text().await.unwrap_or_default();
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"chat_id": chat_id, "message_id": message_id, "status": status.to_string(), "err_body": err_body})), "add ACK reaction failed for chat_id=, message_id=: status=, body=");
            }
        });
    }

    fn http_client(&self) -> reqwest::Client {
        zeroclaw_config::schema::build_channel_proxy_client(
            "channel.telegram",
            self.proxy_url.as_deref(),
        )
    }

    fn normalize_identity(value: &str) -> String {
        value.trim().trim_start_matches('@').to_string()
    }

    /// write a paired user into `peer_groups` and save. The long-running
    /// daemon sets this from the orchestrator; tests and one-shot
    /// callers leave it unset (pairing works at runtime, doesn't persist).
    pub fn with_persistence(mut self, config: Arc<RwLock<Config>>) -> Self {
        self.persist = Some(config);
        self
    }

    /// The daemon `data_dir` via the persistence handle. `None` when
    /// unset: the operator skip path is disabled in that deployment (the
    /// archive error names the cause at skip time).
    fn persisted_data_dir(&self) -> Option<std::path::PathBuf> {
        let persist = self.persist.as_ref()?;
        let data_dir = persist.read().data_dir.clone();
        if data_dir.as_os_str().is_empty() {
            None
        } else {
            Some(data_dir)
        }
    }

    fn operator_skip_marker_for(&self, update_id: i64) -> Option<TelegramSkipMarker> {
        let data_dir = self.persisted_data_dir()?;
        load_telegram_skip_markers(&data_dir, &self.alias)
            .into_iter()
            .find(|marker| marker.update_id == update_id)
    }

    /// Durably archive a skipped poisoned update before advancing the
    /// offset past it — the payload is preserved for inspection, never
    /// silently dropped. Atomic tmp+rename.
    async fn archive_skipped_update(
        &self,
        update_id: i64,
        marker: &TelegramSkipMarker,
        update: &serde_json::Value,
    ) -> Result<std::path::PathBuf, String> {
        let data_dir = self
            .persisted_data_dir()
            .ok_or_else(|| "data_dir unavailable".to_string())?;
        let dir = data_dir.join("telegram_dead_letters").join(&self.alias);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| format!("create {}: {err}", dir.display()))?;
        let record = serde_json::json!({
            "update_id": update_id,
            "alias": self.alias,
            "reason": marker.reason,
            "marker_created_at_unix": marker.created_at_unix,
            "skipped_at_unix": unix_now_secs(),
            "payload": update,
        });
        let body = serde_json::to_string_pretty(&record).map_err(|err| err.to_string())?;
        let tmp = dir.join(format!("{update_id}.json.tmp"));
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|err| format!("create {}: {err}", tmp.display()))?;
        use tokio::io::AsyncWriteExt as _;
        file.write_all(body.as_bytes())
            .await
            .map_err(|err| format!("write {}: {err}", tmp.display()))?;
        let _ = file.sync_all().await;
        drop(file);
        let path = dir.join(format!("{update_id}.json"));
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|err| format!("rename into {}: {err}", path.display()))?;
        Ok(path)
    }

    async fn persist_allowed_identity(&self, identity: &str) -> anyhow::Result<()> {
        use zeroclaw_config::multi_agent::{PeerGroupConfig, PeerUsername};

        let Some(config) = &self.persist else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"identity": identity})),
                "paired identity not persisted (no persistence handle wired)"
            );
            return Ok(());
        };
        let normalized = Self::normalize_identity(identity);
        if normalized.is_empty() {
            anyhow::bail!("Cannot persist empty Telegram identity");
        }
        let group_name = format!("telegram_{}", self.alias);
        let channel_ref: zeroclaw_config::providers::ChannelRef =
            format!("telegram.{}", self.alias).into();
        let snapshot = {
            let mut cfg = config.write();
            if !cfg.channels.telegram.contains_key(&self.alias) {
                anyhow::bail!(
                    "Missing [channels.telegram.{}] section. Run `zeroclaw config set channels.telegram.<alias>.bot_token <token>` to configure.",
                    self.alias
                );
            }
            let group = cfg
                .peer_groups
                .entry(group_name)
                .or_insert_with(|| PeerGroupConfig {
                    channel: channel_ref,
                    ..PeerGroupConfig::default()
                });
            if group
                .external_peers
                .iter()
                .any(|p| Self::normalize_identity(p.as_str()) == normalized)
            {
                return Ok(());
            }
            group.external_peers.push(PeerUsername::new(normalized));
            cfg.clone()
        };
        snapshot
            .save()
            .await
            .context("Failed to persist Telegram peer to config.toml")?;
        Ok(())
    }

    fn extract_bind_code(text: &str) -> Option<&str> {
        let mut parts = text.split_whitespace();
        let command = parts.next()?;
        let base_command = command.split('@').next().unwrap_or(command);
        if base_command != TELEGRAM_BIND_COMMAND {
            return None;
        }
        parts.next().map(str::trim).filter(|code| !code.is_empty())
    }

    fn pairing_code_active(&self) -> bool {
        self.pairing
            .as_ref()
            .and_then(PairingGuard::pairing_code)
            .is_some()
    }

    /// Build the operator-facing `zeroclaw channel bind-telegram` command for
    /// this channel's alias. The CLI defaults to the `default` alias, so only
    /// non-default aliases need the explicit `--alias` flag — emitting it for
    /// the default case would just be noise.
    fn suggested_bind_command(alias: &str, identity: &str) -> String {
        if alias == "default" {
            format!("zeroclaw channel bind-telegram {identity}")
        } else {
            format!("zeroclaw channel bind-telegram {identity} --alias {alias}")
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.bot_token)
    }

    /// Register the bot's slash commands with Telegram via `setMyCommands`.
    /// Called once at startup so that users see a command menu when pressing `/`.
    /// Includes built-in runtime commands, user-installed skill commands, and
    /// enabled tool commands from the configuration.
    async fn register_bot_commands(&self) {
        let mut commands: Vec<serde_json::Value> = vec![
            serde_json::json!({ "command": "new",    "description": "Start a new conversation session" }),
            serde_json::json!({ "command": "clear",  "description": "Clear this conversation session" }),
            serde_json::json!({ "command": "stop",   "description": "Cancel the current in-flight task" }),
            serde_json::json!({ "command": "model",  "description": "Show or switch the current model" }),
            serde_json::json!({ "command": "models", "description": "List available model_providers or switch model_provider" }),
            serde_json::json!({ "command": "config", "description": "Show current configuration" }),
        ];

        // Track registered names to deduplicate across skills and tools.
        let mut used_names: std::collections::HashSet<String> = commands
            .iter()
            .filter_map(|c| c.get("command").and_then(|v| v.as_str()).map(String::from))
            .collect();

        // Collect commands from installed skills.
        if let Some(ref workspace_dir) = self.workspace_dir {
            let skills = zeroclaw_runtime::skills::load_skills(workspace_dir);

            for skill in &skills {
                let sanitized = sanitize_telegram_command_name(&skill.name);
                if sanitized.is_empty() {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!(
                            "Skipping skill '{}': name produces empty Telegram command",
                            skill.name
                        )
                    );
                    continue;
                }
                if used_names.contains(&sanitized) {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!(
                            "Skipping skill '{}': command /{sanitized} conflicts with an existing command",
                            skill.name
                        )
                    );
                    continue;
                }
                let description = if skill.description.is_empty() {
                    format!("Run the {name} skill", name = skill.name)
                } else {
                    truncate_telegram_command_description(&skill.description)
                };
                used_names.insert(sanitized.clone());
                commands.push(serde_json::json!({
                    "command": sanitized,
                    "description": description,
                }));
            }
        }

        // Collect commands from enabled tools.
        for (name, description) in &self.tool_command_specs {
            let sanitized = sanitize_telegram_command_name(name);
            if sanitized.is_empty() || used_names.contains(&sanitized) {
                continue;
            }
            used_names.insert(sanitized.clone());
            commands.push(serde_json::json!({
                "command": sanitized,
                "description": truncate_telegram_command_description(description),
            }));
        }

        // Telegram allows at most 100 commands.
        let total_before_cap = commands.len();
        commands.truncate(TELEGRAM_MAX_BOT_COMMANDS);
        if total_before_cap > TELEGRAM_MAX_BOT_COMMANDS {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"TELEGRAM_MAX_BOT_COMMANDS": TELEGRAM_MAX_BOT_COMMANDS, "total_before_cap": total_before_cap})), "Telegram limits bots to commands; configured, registering first . Reduce installed skills to expose more commands.");
        }

        let url = self.api_url("setMyCommands");
        let body = serde_json::json!({ "commands": commands });

        match self.http_client().post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!(
                        "Telegram bot commands registered successfully ({} commands)",
                        commands.len()
                    )
                );
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"status": status.to_string(), "text": text})
                        ),
                    "Failed to register Telegram bot commands:"
                );
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "Failed to register Telegram bot commands"
                );
            }
        }
    }

    fn is_voice_chat(&self, recipient: &str) -> bool {
        self.voice_chats
            .lock()
            .map(|vs| vs.contains(recipient))
            .unwrap_or(false)
            || (self.voice_peer_resolver)().iter().any(|p| p == recipient)
    }

    fn try_queue_voice_reply(&self, recipient: &str, content: &str, immediate: bool, force: bool) {
        if (!force && !self.is_voice_chat(recipient)) || self.tts_manager.is_none() {
            return;
        }

        // Only queue substantive natural-language replies for voice.
        // Skip tool outputs: URLs, JSON, code blocks, errors, short status.
        let is_substantive = content.len() > 40
            && !content.starts_with("http")
            && !content.starts_with('{')
            && !content.starts_with('[')
            && !content.starts_with("Error")
            && !content.contains("```")
            && !content.contains("tool_call")
            && !content.contains("wttr.in");

        if !is_substantive {
            return;
        }

        let (chat_id, thread_id) = Self::parse_reply_target(recipient);
        let voice_chats = self.voice_chats.clone();
        let voice_peer_resolver = self.voice_peer_resolver.clone();
        let api_base = self.api_base.clone();
        let bot_token = self.bot_token.clone();
        let tts_manager = self.tts_manager.clone().unwrap();

        if immediate {
            // Finalize path: text is already the final answer — no debounce.
            let text = content.to_string();
            let recipient = recipient.to_string();
            zeroclaw_spawn::spawn!(async move {
                let is_config_voice_peer = voice_peer_resolver().contains(&recipient);
                if !is_config_voice_peer && let Ok(mut vc) = voice_chats.lock() {
                    vc.remove(&recipient);
                }
                match Self::synthesize_and_send_voice(
                    &api_base,
                    &bot_token,
                    &chat_id,
                    thread_id.as_deref(),
                    &text,
                    &tts_manager,
                )
                .await
                {
                    Ok(()) => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            ),
                            &format!("voice reply sent ({} chars)", text.len())
                        );
                    }
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                            "TTS voice reply failed"
                        );
                    }
                }
            });
            return;
        }

        // Send path: debounce to coalesce multi-part tool-chain responses.
        if let Ok(mut pv) = self.pending_voice.lock() {
            pv.insert(
                recipient.to_string(),
                (content.to_string(), std::time::Instant::now()),
            );
        }

        let pending = self.pending_voice.clone();
        let recipient = recipient.to_string();
        zeroclaw_spawn::spawn!(async move {
            // Wait 10 seconds — long enough for the agent to finish its
            // full tool chain and send the final answer.
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

            // Atomic check-and-remove: only one task gets the value
            let to_voice = pending.lock().ok().and_then(|mut pv| {
                if let Some((_, ts)) = pv.get(&recipient)
                    && ts.elapsed().as_secs() >= 8
                {
                    return pv.remove(&recipient).map(|(text, _)| text);
                }
                None
            });

            if let Some(text) = to_voice {
                let is_config_voice_peer = voice_peer_resolver().contains(&recipient);
                if !is_config_voice_peer && let Ok(mut vc) = voice_chats.lock() {
                    vc.remove(&recipient);
                }
                match Self::synthesize_and_send_voice(
                    &api_base,
                    &bot_token,
                    &chat_id,
                    thread_id.as_deref(),
                    &text,
                    &tts_manager,
                )
                .await
                {
                    Ok(()) => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            ),
                            &format!("voice reply sent ({} chars)", text.len())
                        );
                    }
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                            "TTS voice reply failed"
                        );
                    }
                }
            }
        });
    }

    /// Synthesize text to speech and send as a Telegram voice note (static version for spawned tasks).
    async fn synthesize_and_send_voice(
        api_base: &str,
        bot_token: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        text: &str,
        tts_manager: &crate::tts::TtsManager,
    ) -> anyhow::Result<()> {
        let audio_bytes = tts_manager.synthesize_opus(text).await?;
        let audio_len = audio_bytes.len();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"audio_len": audio_len})),
            "synthesized bytes of audio"
        );

        if audio_bytes.is_empty() {
            anyhow::bail!("TTS returned empty audio");
        }

        // synthesize_opus already transcodes to OGG/Opus via ffmpeg internally
        let (method, field, filename, mime) = telegram_audio_send_spec("opus")?;

        let url = format!("{api_base}/bot{bot_token}/{method}");
        let client = zeroclaw_config::schema::build_runtime_proxy_client("channel.telegram");

        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                field,
                reqwest::multipart::Part::bytes(audio_bytes)
                    .file_name(filename)
                    .mime_str(mime)?,
            );

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        let resp = client.post(&url).multipart(form).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("{method} failed: status={status}, body={body}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"audio_len": audio_len})),
            "sent voice note ( bytes)"
        );
        Ok(())
    }

    async fn classify_edit_message_response(resp: reqwest::Response) -> EditMessageResult {
        if resp.status().is_success() {
            return EditMessageResult::Success;
        }

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if body.contains("message is not modified") {
            return EditMessageResult::NotModified;
        }

        EditMessageResult::Failed(status)
    }

    async fn fetch_bot_username(&self) -> anyhow::Result<String> {
        let resp = self.http_client().get(self.api_url("getMe")).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to fetch bot info: {}", resp.status());
        }

        let data: serde_json::Value = resp.json().await?;
        let result = data
            .get("result")
            .context("missing result in getMe response")?;
        let username = result
            .get("username")
            .and_then(|u| u.as_str())
            .context("Bot username not found in response")?;

        // Cache the bot's user ID for reply-to-self detection
        if let Some(id) = result.get("id").and_then(|i| i.as_i64()) {
            let mut cache = self.bot_id.lock();
            *cache = Some(id);
        }

        Ok(username.to_string())
    }

    async fn get_bot_username(&self) -> Option<String> {
        {
            let cache = self.bot_username.lock();
            if let Some(ref username) = *cache {
                return Some(username.clone());
            }
        }

        match self.fetch_bot_username().await {
            Ok(username) => {
                let mut cache = self.bot_username.lock();
                *cache = Some(username.clone());
                Some(username)
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "Failed to fetch bot username"
                );
                None
            }
        }
    }

    fn is_telegram_username_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_'
    }

    fn find_bot_mention_spans(text: &str, bot_username: &str) -> Vec<(usize, usize)> {
        let bot_username = bot_username.trim_start_matches('@');
        if bot_username.is_empty() {
            return Vec::new();
        }

        let mut spans = Vec::new();

        for (at_idx, ch) in text.char_indices() {
            if ch != '@' {
                continue;
            }

            if at_idx > 0 {
                let prev = text[..at_idx].chars().next_back().unwrap_or(' ');
                if Self::is_telegram_username_char(prev) {
                    continue;
                }
            }

            let username_start = at_idx + 1;
            let mut username_end = username_start;

            for (rel_idx, candidate_ch) in text[username_start..].char_indices() {
                if Self::is_telegram_username_char(candidate_ch) {
                    username_end = username_start + rel_idx + candidate_ch.len_utf8();
                } else {
                    break;
                }
            }

            if username_end == username_start {
                continue;
            }

            let mention_username = &text[username_start..username_end];
            if mention_username.eq_ignore_ascii_case(bot_username) {
                spans.push((at_idx, username_end));
            }
        }

        spans
    }

    fn contains_bot_mention(text: &str, bot_username: &str) -> bool {
        !Self::find_bot_mention_spans(text, bot_username).is_empty()
    }

    fn normalize_incoming_content(text: &str, _bot_username: &str) -> Option<String> {
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn is_group_message(message: &serde_json::Value) -> bool {
        message
            .get("chat")
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str())
            .map(|t| t == "group" || t == "supergroup")
            .unwrap_or(false)
    }

    /// Check whether `message` is a reply to a message sent by the bot
    /// itself. When true, the `mention_only` gate should be bypassed.
    fn is_reply_to_bot(message: &serde_json::Value, bot_id: i64) -> bool {
        message
            .get("reply_to_message")
            .and_then(|r| r.get("from"))
            .and_then(|f| f.get("id"))
            .and_then(|i| i.as_i64())
            .is_some_and(|id| id == bot_id)
    }

    fn check_media_mention_gate(
        &self,
        message: &serde_json::Value,
        caption: Option<&str>,
    ) -> Option<Option<String>> {
        let is_group = Self::is_group_message(message);
        if !self.mention_only || !is_group {
            return Some(caption.map(String::from));
        }
        let bot_username_guard = self.bot_username.lock();
        let bot_username = bot_username_guard.as_ref()?;

        // If the user is replying directly to the bot's message, bypass the
        // mention check — replies are an unambiguous signal of intent.
        if let Some(caption) = caption
            && let Some(bot_id) = *self.bot_id.lock()
            && Self::is_reply_to_bot(message, bot_id)
        {
            return Some(Self::normalize_incoming_content(caption, bot_username));
        }

        let caption = caption?;
        if !Self::contains_bot_mention(caption, bot_username) {
            return None;
        }
        Some(Self::normalize_incoming_content(caption, bot_username))
    }

    fn is_user_allowed(&self, username: &str) -> bool {
        let identity = Self::normalize_identity(username);
        let peers: Vec<String> = (self.peer_resolver)()
            .into_iter()
            .map(|p| Self::normalize_identity(&p))
            .filter(|p| !p.is_empty())
            .collect();
        crate::allowlist::is_user_allowed(&peers, &identity, crate::allowlist::Match::Sensitive)
    }

    fn is_any_user_allowed<'a, I>(&self, identities: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        identities.into_iter().any(|id| self.is_user_allowed(id))
    }

    async fn handle_unauthorized_message(&self, update: &serde_json::Value) {
        let Some(message) = update.get("message") else {
            return;
        };

        let Some(text) = message.get("text").and_then(serde_json::Value::as_str) else {
            return;
        };

        let username_opt = message
            .get("from")
            .and_then(|from| from.get("username"))
            .and_then(serde_json::Value::as_str);
        let username = username_opt.unwrap_or("unknown");
        let normalized_username = Self::normalize_identity(username);

        let sender_id = message
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(serde_json::Value::as_i64);
        let sender_id_str = sender_id.map(|id| id.to_string());
        let normalized_sender_id = sender_id_str.as_deref().map(Self::normalize_identity);

        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        let Some(chat_id) = chat_id else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "missing chat_id in message, skipping"
            );
            return;
        };

        let mut identities = vec![normalized_username.as_str()];
        if let Some(ref id) = normalized_sender_id {
            identities.push(id.as_str());
        }

        if self.is_any_user_allowed(identities.iter().copied()) {
            return;
        }

        if let Some(code) = Self::extract_bind_code(text) {
            if let Some(pairing) = self.pairing.as_ref() {
                match pairing.try_pair(code, &chat_id).await {
                    Ok(Some(_token)) => {
                        let bind_identity = normalized_sender_id.clone().or_else(|| {
                            if normalized_username.is_empty() || normalized_username == "unknown" {
                                None
                            } else {
                                Some(normalized_username.clone())
                            }
                        });

                        if let Some(identity) = bind_identity {
                            match Box::pin(self.persist_allowed_identity(&identity)).await {
                                Ok(()) => {
                                    let _ = self
                                        .send(&SendMessage::new(
                                            "✅ Telegram account bound successfully. You can talk to ZeroClaw now.",
                                            &chat_id,
                                        ))
                                        .await;
                                    ::zeroclaw_log::record!(
                                        INFO,
                                        ::zeroclaw_log::Event::new(
                                            module_path!(),
                                            ::zeroclaw_log::Action::Note
                                        )
                                        .with_attrs(::serde_json::json!({"identity": identity})),
                                        "paired and allowlisted identity="
                                    );
                                }
                                Err(e) => {
                                    ::zeroclaw_log::record!(
                                        ERROR,
                                        ::zeroclaw_log::Event::new(
                                            module_path!(),
                                            ::zeroclaw_log::Action::Fail
                                        )
                                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                        .with_attrs(::serde_json::json!({"e": e.to_string()})),
                                        "failed to persist allowlist after bind"
                                    );
                                    let _ = self
                                        .send(&SendMessage::new(
                                            "⚠️ Bound for this runtime, but failed to persist config. Access may be lost after restart; check config file permissions.",
                                            &chat_id,
                                        ))
                                        .await;
                                }
                            }
                        } else {
                            let _ = self
                                .send(&SendMessage::new(
                                    "❌ Could not identify your Telegram account. Ensure your account has a username or stable user ID, then retry.",
                                    &chat_id,
                                ))
                                .await;
                        }
                    }
                    Ok(None) => {
                        let _ = self
                            .send(&SendMessage::new(
                                "❌ Invalid binding code. Ask operator for the latest code and retry.",
                                &chat_id,
                            ))
                            .await;
                    }
                    Err(lockout_secs) => {
                        let _ = self
                            .send(&SendMessage::new(
                                format!("⏳ Too many invalid attempts. Retry in {lockout_secs}s."),
                                &chat_id,
                            ))
                            .await;
                    }
                }
            } else {
                let _ = self
                    .send(&SendMessage::new(
                        "ℹ️ Telegram pairing is not active. Ask operator to add your user ID to the matching peer_groups.telegram_<alias>.external_peers entry in config.toml.",
                        &chat_id,
                    ))
                    .await;
            }
            return;
        }

        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            &format!(
                "ignoring message from unauthorized user: username={username}, sender_id={}. \
Allowlist Telegram username (without '@') or numeric user ID.",
                sender_id_str.as_deref().unwrap_or("unknown")
            )
        );

        let suggested_identity = normalized_sender_id
            .clone()
            .or_else(|| {
                if normalized_username.is_empty() || normalized_username == "unknown" {
                    None
                } else {
                    Some(normalized_username.clone())
                }
            })
            .unwrap_or_else(|| "YOUR_TELEGRAM_ID".to_string());

        // Emit the bind command scoped to THIS channel's alias. The CLI
        // handler defaults to the `default` alias, so an alias-less command
        // would silently bind the wrong peer group for a non-default agent
        // and the bot would keep demanding approval.
        let bind_command = Self::suggested_bind_command(&self.alias, &suggested_identity);

        let _ = self
            .send(&SendMessage::new(
                format!(
                    "🔐 This bot requires operator approval.\n\nCopy this command to the operator terminal:\n`{bind_command}`\n\nAfter the operator runs it, send your message again."
                ),
                &chat_id,
            ))
            .await;

        // Only offer the `/bind <code>` path while the channel is genuinely
        // unpaired. Once peers exist (resolved live), the one-time code is
        // moot and the hint just confuses an operator who already authorized
        // someone — the "already assigned but still asks" complaint.
        if self.pairing_code_active() && (self.peer_resolver)().is_empty() {
            let _ = self
                .send(&SendMessage::new(
                    "ℹ️ If the operator provides a one-time pairing code, you can also run `/bind <code>`.",
                    &chat_id,
                ))
                .await;
        }
    }

    /// Get the file path for a Telegram file ID via the Bot API.
    ///
    /// Failures carry the vendor's HTTP status, `ok` flag, `error_code`, and
    /// `description`, classified as [`FileLookupFailure::Permanent`] or
    /// `Transient` so the caller can acknowledge an update whose download can
    /// never succeed instead of retrying it forever.
    async fn get_file_path(&self, file_id: &str) -> Result<String, FileLookupError> {
        let url = self.api_url("getFile");
        let resp = match self
            .http_client()
            .get(&url)
            .query(&[("file_id", file_id)])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(FileLookupError::transient(format!(
                    "Failed to call Telegram getFile: {e}"
                )));
            }
        };

        let status = resp.status();
        let body: Option<serde_json::Value> = resp.json().await.ok();

        if let Some(path) = body
            .as_ref()
            .and_then(|b| b.get("result"))
            .and_then(|r| r.get("file_path"))
            .and_then(serde_json::Value::as_str)
        {
            return Ok(path.to_string());
        }

        Err(FileLookupError::classify(status, body.as_ref()))
    }

    /// Download a file from the Telegram CDN.
    async fn download_file(&self, file_path: &str) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/file/bot{}/{file_path}", self.api_base, self.bot_token);
        let resp = self
            .http_client()
            .get(&url)
            .send()
            .await
            .context("Failed to download Telegram file")?;

        if !resp.status().is_success() {
            anyhow::bail!("Telegram file download failed: {}", resp.status());
        }

        Ok(resp.bytes().await?.to_vec())
    }

    /// Extract (file_id, duration) from a voice or audio message.
    fn parse_voice_metadata(message: &serde_json::Value) -> Option<(String, u64)> {
        let voice = message.get("voice").or_else(|| message.get("audio"))?;
        let file_id = voice.get("file_id")?.as_str()?.to_string();
        let duration = voice
            .get("duration")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Some((file_id, duration))
    }

    /// Extract attachment metadata from an incoming Telegram message (document or photo).
    /// Returns `None` for text-only, voice, and other unsupported message types.
    fn parse_attachment_metadata(message: &serde_json::Value) -> Option<IncomingAttachment> {
        // Try document first
        if let Some(doc) = message.get("document") {
            let file_id = doc.get("file_id")?.as_str()?.to_string();
            let file_name = doc
                .get("file_name")
                .and_then(serde_json::Value::as_str)
                .map(String::from);
            let file_size = doc.get("file_size").and_then(serde_json::Value::as_u64);
            let caption = message
                .get("caption")
                .and_then(serde_json::Value::as_str)
                .map(String::from);
            return Some(IncomingAttachment {
                file_id,
                file_name,
                file_size,
                caption,
                kind: IncomingAttachmentKind::Document,
            });
        }

        // Try photo (array of PhotoSize, take last = highest resolution)
        if let Some(photos) = message.get("photo").and_then(serde_json::Value::as_array) {
            let best = photos.last()?;
            let file_id = best.get("file_id")?.as_str()?.to_string();
            let file_size = best.get("file_size").and_then(serde_json::Value::as_u64);
            let caption = message
                .get("caption")
                .and_then(serde_json::Value::as_str)
                .map(String::from);
            return Some(IncomingAttachment {
                file_id,
                file_name: None,
                file_size,
                caption,
                kind: IncomingAttachmentKind::Photo,
            });
        }

        None
    }

    async fn try_parse_attachment_message(&self, update: &serde_json::Value) -> UpdateDisposition {
        let Some(message) = update.get("message") else {
            return UpdateDisposition::SkipPermanent;
        };
        let Some(attachment) = Self::parse_attachment_metadata(message) else {
            return UpdateDisposition::SkipPermanent;
        };

        // Check file size limit
        if let Some(size) = attachment.file_size
            && size > TELEGRAM_MAX_FILE_DOWNLOAD_BYTES
        {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!(
                    "Skipping attachment: file size {size} bytes exceeds {} MB limit",
                    TELEGRAM_MAX_FILE_DOWNLOAD_BYTES / (1024 * 1024)
                )
            );
            return UpdateDisposition::SkipPermanent;
        }

        let (username, sender_id, sender_identity) = Self::extract_sender_info(message);

        let mut identities = vec![username.as_str()];
        if let Some(id) = sender_id.as_deref() {
            identities.push(id);
        }

        if !self.is_any_user_allowed(identities.iter().copied()) {
            return UpdateDisposition::SkipPermanent;
        }

        // Apply mention_only gate before downloading. Photo / document
        // updates carry no `text` field, so the text-only gate in
        // `parse_update_message` can never see them and they used to slip
        // through unconditionally.
        let Some(gated_caption) =
            self.check_media_mention_gate(message, attachment.caption.as_deref())
        else {
            return UpdateDisposition::SkipPermanent;
        };

        let Some(chat_id) = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string())
        else {
            return UpdateDisposition::SkipPermanent;
        };

        let message_id = message
            .get("message_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        let thread_id = message
            .get("message_thread_id")
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        let reply_target = if let Some(ref tid) = thread_id {
            format!("{}:{}", chat_id, tid)
        } else {
            chat_id.clone()
        };

        // Ensure workspace directory is configured
        let Some(workspace) = self.workspace_dir.as_ref().or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Cannot save attachment: workspace_dir not configured"
            );
            None
        }) else {
            return UpdateDisposition::SkipPermanent;
        };

        let save_dir = workspace.join("telegram_files");
        if let Err(e) = tokio::fs::create_dir_all(&save_dir).await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                "Failed to create telegram_files directory"
            );
            return UpdateDisposition::RetryTransient;
        }

        // Download file from Telegram
        let tg_file_path = match self.get_file_path(&attachment.file_id).await {
            Ok(p) => p,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "error": zeroclaw_runtime::security::scrub(&format!("{}", e)),
                            "classification": format!("{:?}", e.kind),
                        })),
                    "Failed to get attachment file path"
                );
                return match e.kind {
                    FileLookupFailure::Permanent => UpdateDisposition::SkipPermanent,
                    FileLookupFailure::Transient => UpdateDisposition::RetryTransient,
                };
            }
        };

        let file_data = match self.download_file(&tg_file_path).await {
            Ok(d) => d,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "Failed to download attachment"
                );
                return UpdateDisposition::RetryTransient;
            }
        };

        // Determine local filename
        let local_filename = match &attachment.file_name {
            Some(name) => name.clone(),
            None => {
                // For photos, derive extension from Telegram file path
                let ext = tg_file_path.rsplit('.').next().unwrap_or("jpg");
                format!("photo_{chat_id}_{message_id}.{ext}")
            }
        };

        let local_path = save_dir.join(&local_filename);
        if let Err(e) = tokio::fs::write(&local_path, &file_data).await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                &format!("Failed to save attachment to {}", local_path.display())
            );
            return UpdateDisposition::RetryTransient;
        }

        // Build message content.
        // Photos with image extensions use [IMAGE:] marker so the multimodal
        // pipeline validates vision capability. Non-image files always get
        // [Document:] format regardless of Telegram's classification.
        let mut content = format_attachment_content(attachment.kind, &local_filename, &local_path);
        // `gated_caption` is the trimmed caption when the `mention_only`
        // gate admits it; otherwise the raw caption (or None).
        if let Some(caption) = gated_caption.as_deref()
            && !caption.is_empty()
        {
            use std::fmt::Write;
            let _ = write!(content, "\n\n{caption}");
        }

        // Prepend reply context if replying to another message
        if let Some(quote) = self.extract_reply_context(message) {
            content = format!("{quote}\n\n{content}");
        }

        // Prepend forwarding attribution when the message was forwarded
        if let Some(attr) = Self::format_forward_attribution(message) {
            content = Self::prepend_forward_attribution(&attr, content);
        }

        UpdateDisposition::Parsed(Box::new(ChannelMessage {
            id: format!("telegram_{chat_id}_{message_id}"),
            sender: sender_identity,
            reply_target,
            content,
            channel: "telegram".into(),
            channel_alias: Some(self.alias.clone()),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: thread_id,
            interruption_scope_id: None,
            attachments: vec![],
            subject: None,

            ..Default::default()
        }))
    }

    /// Attempt to parse a Telegram update as a voice message and transcribe it.
    /// Returns `SkipPermanent` if the message is not a voice message, transcription is
    /// disabled, or the message exceeds duration limits; `RetryTransient` if download or
    /// transcription I/O fails.
    async fn try_parse_voice_message(&self, update: &serde_json::Value) -> UpdateDisposition {
        let Some(config) = self.transcription.as_ref() else {
            return UpdateDisposition::SkipPermanent;
        };
        let Some(manager) = self.transcription_manager.as_deref() else {
            return UpdateDisposition::SkipPermanent;
        };
        let Some(message) = update.get("message") else {
            return UpdateDisposition::SkipPermanent;
        };

        let Some((file_id, duration)) = Self::parse_voice_metadata(message) else {
            return UpdateDisposition::SkipPermanent;
        };

        if duration > config.max_duration_secs {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!(
                    "Skipping voice message: duration {duration}s exceeds limit {}s",
                    config.max_duration_secs
                )
            );
            return UpdateDisposition::SkipPermanent;
        }

        let (username, sender_id, sender_identity) = Self::extract_sender_info(message);

        let mut identities = vec![username.as_str()];
        if let Some(id) = sender_id.as_deref() {
            identities.push(id);
        }

        if !self.is_any_user_allowed(identities.iter().copied()) {
            return UpdateDisposition::SkipPermanent;
        }

        let voice_caption = message.get("caption").and_then(serde_json::Value::as_str);
        if self
            .check_media_mention_gate(message, voice_caption)
            .is_none()
        {
            return UpdateDisposition::SkipPermanent;
        }

        let Some(chat_id) = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string())
        else {
            return UpdateDisposition::SkipPermanent;
        };

        let message_id = message
            .get("message_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        let thread_id = message
            .get("message_thread_id")
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        let reply_target = if let Some(ref tid) = thread_id {
            format!("{}:{}", chat_id, tid)
        } else {
            chat_id.clone()
        };

        // Download and transcribe
        let file_path = match self.get_file_path(&file_id).await {
            Ok(p) => p,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "error": zeroclaw_runtime::security::scrub(&format!("{}", e)),
                            "classification": format!("{:?}", e.kind),
                        })),
                    "Failed to get voice file path"
                );
                return match e.kind {
                    FileLookupFailure::Permanent => UpdateDisposition::SkipPermanent,
                    FileLookupFailure::Transient => UpdateDisposition::RetryTransient,
                };
            }
        };

        let file_name = file_path
            .rsplit('/')
            .next()
            .unwrap_or("voice.ogg")
            .to_string();

        let audio_data = match self.download_file(&file_path).await {
            Ok(d) => d,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "Failed to download voice file"
                );
                return UpdateDisposition::RetryTransient;
            }
        };

        let text = match manager.transcribe(&audio_data, &file_name).await {
            Ok(t) => t,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "Voice transcription failed"
                );
                return UpdateDisposition::RetryTransient;
            }
        };

        if text.trim().is_empty() {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                "Voice transcription returned empty text, skipping"
            );
            return UpdateDisposition::SkipPermanent;
        }

        // Enter voice-chat mode so outgoing replies get a TTS voice note
        if let Ok(mut vc) = self.voice_chats.lock() {
            vc.insert(reply_target.clone());
        }

        // Cache transcription for reply-context lookups
        {
            let mut cache = self.voice_transcriptions.lock();
            if cache.len() >= 100 {
                cache.clear();
            }
            cache.insert(format!("{chat_id}:{message_id}"), text.clone());
        }

        let content = if let Some(quote) = self.extract_reply_context(message) {
            format!("{quote}\n\n[Voice] {text}")
        } else {
            format!("[Voice] {text}")
        };

        // Prepend forwarding attribution when the message was forwarded
        let content = if let Some(attr) = Self::format_forward_attribution(message) {
            Self::prepend_forward_attribution(&attr, content)
        } else {
            content
        };

        UpdateDisposition::Parsed(Box::new(ChannelMessage {
            id: format!("telegram_{chat_id}_{message_id}"),
            sender: sender_identity,
            reply_target,
            content,
            channel: "telegram".into(),
            channel_alias: Some(self.alias.clone()),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: thread_id,
            interruption_scope_id: None,
            attachments: vec![],
            subject: None,

            ..Default::default()
        }))
    }

    /// Extract sender username and display identity from a Telegram message object.
    fn extract_sender_info(message: &serde_json::Value) -> (String, Option<String>, String) {
        let username = message
            .get("from")
            .and_then(|from| from.get("username"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let sender_id = message
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());
        let sender_identity = if username == "unknown" {
            sender_id.clone().unwrap_or_else(|| "unknown".to_string())
        } else {
            username.clone()
        };
        (username, sender_id, sender_identity)
    }

    /// Build a forwarding attribution prefix from Telegram forward fields.
    /// Returns `Some("[Forwarded from ...] ")` when the message is forwarded,
    /// `None` otherwise.
    fn format_forward_attribution(message: &serde_json::Value) -> Option<String> {
        if let Some(origin) = message.get("forward_origin") {
            let origin_type = origin.get("type").and_then(serde_json::Value::as_str)?;
            let label = match origin_type {
                "user" => {
                    let sender = origin.get("sender_user")?;
                    Self::format_forwarded_user_label(sender, "unknown")
                }
                "hidden_user" => origin
                    .get("sender_user_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown hidden user")
                    .to_string(),
                "chat" => {
                    let title = origin
                        .get("sender_chat")
                        .and_then(|chat| chat.get("title"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown chat");
                    format!("chat: {title}")
                }
                "channel" => {
                    let title = origin
                        .get("chat")
                        .and_then(|chat| chat.get("title"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown channel");
                    format!("channel: {title}")
                }
                _ => "unknown source".to_string(),
            };
            Some(format!("[Forwarded from {label}] "))
        } else if let Some(from_chat) = message.get("forward_from_chat") {
            // Forwarded from a channel or group
            let title = from_chat
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown channel");
            Some(format!("[Forwarded from channel: {title}] "))
        } else if let Some(from_user) = message.get("forward_from") {
            // Forwarded from a user (privacy allows identity)
            let label = Self::format_forwarded_user_label(from_user, "unknown");
            Some(format!("[Forwarded from {label}] "))
        } else {
            // Forwarded from a user who hides their identity
            message
                .get("forward_sender_name")
                .and_then(serde_json::Value::as_str)
                .map(|name| format!("[Forwarded from {name}] "))
        }
    }

    fn prepend_forward_attribution(attr: &str, content: String) -> String {
        let attr = attr.trim_end();
        if content.starts_with("> ") {
            format!("{attr}\n\n{content}")
        } else {
            format!("{attr} {content}")
        }
    }

    fn format_forwarded_user_label(user: &serde_json::Value, fallback: &str) -> String {
        if let Some(username) = user.get("username").and_then(serde_json::Value::as_str) {
            return format!("@{username}");
        }

        let Some(first_name) = user.get("first_name").and_then(serde_json::Value::as_str) else {
            return fallback.to_string();
        };

        let mut label = first_name.to_string();
        if let Some(last_name) = user.get("last_name").and_then(serde_json::Value::as_str) {
            label.push(' ');
            label.push_str(last_name);
        }
        label
    }

    /// Extract reply context from a Telegram `reply_to_message`, if present.
    fn extract_reply_context(&self, message: &serde_json::Value) -> Option<String> {
        let reply = message.get("reply_to_message")?;

        let reply_mid = reply.get("message_id").and_then(serde_json::Value::as_i64);
        let thread_id = message
            .get("message_thread_id")
            .and_then(serde_json::Value::as_i64);
        if let (Some(rmid), Some(tid)) = (reply_mid, thread_id)
            && rmid == tid
        {
            return None;
        }

        let reply_sender = reply
            .get("from")
            .and_then(|from| from.get("username"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                reply
                    .get("from")
                    .and_then(|from| from.get("first_name"))
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or("unknown");

        let reply_text = if let Some(text) = reply.get("text").and_then(serde_json::Value::as_str) {
            text.to_string()
        } else if reply.get("voice").is_some() || reply.get("audio").is_some() {
            let reply_mid = reply.get("message_id").and_then(serde_json::Value::as_i64);
            let chat_id = message
                .get("chat")
                .and_then(|c| c.get("id"))
                .and_then(serde_json::Value::as_i64);
            if let (Some(mid), Some(cid)) = (reply_mid, chat_id) {
                self.voice_transcriptions
                    .lock()
                    .get(&format!("{cid}:{mid}"))
                    .map(|t| format!("[Voice] {t}"))
                    .unwrap_or_else(|| "[Voice message]".to_string())
            } else {
                "[Voice message]".to_string()
            }
        } else if reply.get("photo").is_some() {
            "[Photo]".to_string()
        } else if reply.get("document").is_some() {
            "[Document]".to_string()
        } else if reply.get("video").is_some() {
            "[Video]".to_string()
        } else if reply.get("sticker").is_some() {
            "[Sticker]".to_string()
        } else {
            "[Message]".to_string()
        };

        // Format as blockquote with sender attribution
        let quoted_lines: String = reply_text
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        Some(format!("> @{reply_sender}:\n{quoted_lines}"))
    }

    fn parse_update_message(&self, update: &serde_json::Value) -> Option<ChannelMessage> {
        let message = update.get("message")?;

        let text = message.get("text").and_then(serde_json::Value::as_str)?;

        let (username, sender_id, sender_identity) = Self::extract_sender_info(message);

        let mut identities = vec![username.as_str()];
        if let Some(id) = sender_id.as_deref() {
            identities.push(id);
        }

        if !self.is_any_user_allowed(identities.iter().copied()) {
            return None;
        }

        let is_group = Self::is_group_message(message);
        if self.mention_only && is_group {
            let bot_username = self.bot_username.lock();
            let bot_username = bot_username.as_ref()?;
            // If the user is replying directly to the bot's message, bypass
            // the mention check — replies are an unambiguous signal of intent.
            if !Self::contains_bot_mention(text, bot_username) {
                let bot_id = *self.bot_id.lock();
                if bot_id.is_none_or(|id| !Self::is_reply_to_bot(message, id)) {
                    return None;
                }
            }
        }

        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string())?;

        let message_id = message
            .get("message_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        // Extract thread/topic ID for forum support
        let thread_id = message
            .get("message_thread_id")
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        // reply_target: chat_id or chat_id:thread_id format
        let reply_target = if let Some(ref tid) = thread_id {
            format!("{}:{}", chat_id, tid)
        } else {
            chat_id.clone()
        };

        let content = if self.mention_only && is_group {
            let bot_username = self.bot_username.lock();
            let bot_username = bot_username.as_ref()?;
            Self::normalize_incoming_content(text, bot_username)?
        } else {
            text.to_string()
        };

        let content = if let Some(quote) = self.extract_reply_context(message) {
            format!("{quote}\n\n{content}")
        } else {
            content
        };

        // Prepend forwarding attribution when the message was forwarded
        let content = if let Some(attr) = Self::format_forward_attribution(message) {
            Self::prepend_forward_attribution(&attr, content)
        } else {
            content
        };

        // Exit input-driven voice mode when user switches back to typing.
        // Config-mandated voice peers (output_modality = "voice") stay in
        // voice mode regardless of whether they send text or voice.
        if !self.is_voice_peer(&reply_target)
            && let Ok(mut vc) = self.voice_chats.lock()
        {
            vc.remove(&reply_target);
        }

        Some(ChannelMessage {
            id: format!("telegram_{chat_id}_{message_id}"),
            sender: sender_identity,
            reply_target,
            content,
            channel: "telegram".into(),
            channel_alias: Some(self.alias.clone()),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: thread_id,
            interruption_scope_id: None,
            attachments: vec![],
            subject: None,

            ..Default::default()
        })
    }

    /// Convert Markdown to Telegram HTML format.
    /// Telegram HTML supports: <b>, <i>, <u>, <s>, <code>, <pre>, <a href="...">
    /// This mirrors OpenClaw's markdownToTelegramHtml approach.
    fn markdown_to_telegram_html(text: &str) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut result_lines: Vec<String> = Vec::new();

        for line in &lines {
            let trimmed_line = line.trim_start();
            if trimmed_line.starts_with("```") {
                // Preserve fence lines so the second-pass block parser can consume them
                // without interference from inline backtick handling.
                result_lines.push(trimmed_line.to_string());
                continue;
            }

            let mut line_out = String::new();

            // Handle code blocks (``` ... ```) - handled at text level below
            // Handle headers: ## Title → <b>Title</b>
            let stripped = line.trim_start_matches('#');
            let header_level = line.len() - stripped.len();
            if header_level > 0 && line.starts_with('#') && stripped.starts_with(' ') {
                let title = Self::escape_html(stripped.trim());
                result_lines.push(format!("<b>{title}</b>"));
                continue;
            }

            // Inline formatting
            let mut i = 0;
            let bytes = line.as_bytes();
            let len = bytes.len();
            while i < len {
                // Bold: **text** or __text__
                if i + 1 < len
                    && bytes[i] == b'*'
                    && bytes[i + 1] == b'*'
                    && let Some(end) = line[i + 2..].find("**")
                {
                    let inner = Self::escape_html(&line[i + 2..i + 2 + end]);
                    let _ = write!(line_out, "<b>{inner}</b>");
                    i += 4 + end;
                    continue;
                }
                if i + 1 < len
                    && bytes[i] == b'_'
                    && bytes[i + 1] == b'_'
                    && let Some(end) = line[i + 2..].find("__")
                {
                    let inner = Self::escape_html(&line[i + 2..i + 2 + end]);
                    let _ = write!(line_out, "<b>{inner}</b>");
                    i += 4 + end;
                    continue;
                }
                // Italic: *text* or _text_ (single)
                if bytes[i] == b'*'
                    && (i == 0 || bytes[i - 1] != b'*')
                    && let Some(end) = line[i + 1..].find('*')
                    && end > 0
                {
                    let inner = Self::escape_html(&line[i + 1..i + 1 + end]);
                    let _ = write!(line_out, "<i>{inner}</i>");
                    i += 2 + end;
                    continue;
                }
                // Inline code: `code`
                if bytes[i] == b'`'
                    && (i == 0 || bytes[i - 1] != b'`')
                    && let Some(end) = line[i + 1..].find('`')
                {
                    let inner = Self::escape_html(&line[i + 1..i + 1 + end]);
                    let _ = write!(line_out, "<code>{inner}</code>");
                    i += 2 + end;
                    continue;
                }
                // Markdown link: [text](url)
                if bytes[i] == b'['
                    && let Some(bracket_end) = line[i + 1..].find(']')
                {
                    let text_part = &line[i + 1..i + 1 + bracket_end];
                    let after_bracket = i + 1 + bracket_end + 1; // position after ']'
                    if after_bracket < len
                        && bytes[after_bracket] == b'('
                        && let Some(paren_end) = line[after_bracket + 1..].find(')')
                    {
                        let url = &line[after_bracket + 1..after_bracket + 1 + paren_end];
                        if url.starts_with("http://") || url.starts_with("https://") {
                            let text_html = Self::escape_html(text_part);
                            let url_html = Self::escape_html(url);
                            let _ = write!(line_out, "<a href=\"{url_html}\">{text_html}</a>");
                            i = after_bracket + 1 + paren_end + 1;
                            continue;
                        }
                    }
                }
                // Strikethrough: ~~text~~
                if i + 1 < len
                    && bytes[i] == b'~'
                    && bytes[i + 1] == b'~'
                    && let Some(end) = line[i + 2..].find("~~")
                {
                    let inner = Self::escape_html(&line[i + 2..i + 2 + end]);
                    let _ = write!(line_out, "<s>{inner}</s>");
                    i += 4 + end;
                    continue;
                }
                // Default: escape HTML entities
                let ch = line[i..].chars().next().unwrap();
                match ch {
                    '<' => line_out.push_str("&lt;"),
                    '>' => line_out.push_str("&gt;"),
                    '&' => line_out.push_str("&amp;"),
                    '"' => line_out.push_str("&quot;"),
                    '\'' => line_out.push_str("&#39;"),
                    _ => line_out.push(ch),
                }
                i += ch.len_utf8();
            }
            result_lines.push(line_out);
        }

        // Second pass: handle ``` code blocks across lines
        let joined = result_lines.join("\n");
        let mut final_out = String::with_capacity(joined.len());
        let mut in_code_block = false;
        let mut code_buf = String::new();

        for line in joined.split('\n') {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                if in_code_block {
                    in_code_block = false;
                    let escaped = code_buf.trim_end_matches('\n');
                    // Telegram HTML parse mode supports <pre> and <code>, but not class attributes.
                    let _ = writeln!(final_out, "<pre><code>{escaped}</code></pre>");
                    code_buf.clear();
                } else {
                    in_code_block = true;
                    code_buf.clear();
                }
            } else if in_code_block {
                code_buf.push_str(line);
                code_buf.push('\n');
            } else {
                final_out.push_str(line);
                final_out.push('\n');
            }
        }
        if in_code_block && !code_buf.is_empty() {
            let _ = writeln!(final_out, "<pre><code>{}</code></pre>", code_buf.trim_end());
        }

        final_out.trim_end_matches('\n').to_string()
    }

    fn escape_html(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }

    async fn send_text_chunks(
        &self,
        message: &str,
        chat_id: &str,
        thread_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let chunks = split_message_for_telegram(message);

        for (index, chunk) in chunks.iter().enumerate() {
            let text = format_telegram_text_chunk(chunk, index, chunks.len());

            let mut markdown_body = serde_json::json!({
                "chat_id": chat_id,
                "text": Self::markdown_to_telegram_html(&text),
                "parse_mode": "HTML"
            });

            // Add message_thread_id for forum topic support
            if let Some(tid) = thread_id {
                markdown_body["message_thread_id"] = serde_json::Value::String(tid.to_string());
            }

            let markdown_resp = self
                .http_client()
                .post(self.api_url("sendMessage"))
                .json(&markdown_body)
                .send()
                .await?;

            if markdown_resp.status().is_success() {
                if index < chunks.len() - 1 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                continue;
            }

            let markdown_status = markdown_resp.status();
            let markdown_err = markdown_resp.text().await.unwrap_or_default();
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"status": markdown_status.to_string()})),
                "Telegram sendMessage with Markdown failed; retrying without parse_mode"
            );

            let mut plain_body = serde_json::json!({
                "chat_id": chat_id,
                "text": text,
            });

            // Add message_thread_id for forum topic support
            if let Some(tid) = thread_id {
                plain_body["message_thread_id"] = serde_json::Value::String(tid.to_string());
            }
            let plain_resp = self
                .http_client()
                .post(self.api_url("sendMessage"))
                .json(&plain_body)
                .send()
                .await?;

            if !plain_resp.status().is_success() {
                let plain_status = plain_resp.status();
                let plain_err = plain_resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Telegram sendMessage failed (markdown {}: {}; plain {}: {})",
                    markdown_status,
                    markdown_err,
                    plain_status,
                    plain_err
                );
            }

            if index < chunks.len() - 1 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        Ok(())
    }

    async fn send_media_by_url(
        &self,
        method: &str,
        media_field: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
        });
        body[media_field] = serde_json::Value::String(url.to_string());

        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        if let Some(cap) = caption {
            body["caption"] = serde_json::Value::String(cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url(method))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("{method} by URL failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({"method": method, "chat_id": chat_id, "url": url})
            ),
            "sent to"
        );
        Ok(())
    }

    async fn send_attachment(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        attachment: &TelegramAttachment,
    ) -> anyhow::Result<()> {
        let target = attachment.target.trim();

        if is_http_url(target) {
            let result = match attachment.kind {
                TelegramAttachmentKind::Image => {
                    self.send_photo_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Document => {
                    self.send_document_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Video => {
                    self.send_video_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Audio => {
                    self.send_audio_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Voice => {
                    self.send_voice_by_url(chat_id, thread_id, target, None)
                        .await
                }
            };

            // If sending media by URL failed (e.g. Telegram can't fetch the URL,
            // wrong content type, etc.), fall back to sending the URL as a text link
            // instead of losing the reply entirely.
            if let Err(e) = result {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"url": target, "error": zeroclaw_runtime::security::scrub(&format!("{}", e))})
                        ),
                    "Telegram send media by URL failed; falling back to text link"
                );
                let kind_label = match attachment.kind {
                    TelegramAttachmentKind::Image => "Image",
                    TelegramAttachmentKind::Document => "Document",
                    TelegramAttachmentKind::Video => "Video",
                    TelegramAttachmentKind::Audio => "Audio",
                    TelegramAttachmentKind::Voice => "Voice",
                };
                let fallback_text = format!("{kind_label}: {target}");
                self.send_text_chunks(&fallback_text, chat_id, thread_id)
                    .await?;
            }

            return Ok(());
        }

        // Remap Docker container workspace path (/workspace/...) to the host
        // workspace directory so files written by the containerised runtime
        // can be found and sent by the host-side Telegram sender.
        let remapped;
        let target = if let Some(rel) = target.strip_prefix("/workspace/") {
            if let Some(ws) = &self.workspace_dir {
                remapped = ws.join(rel);
                remapped.to_str().unwrap_or(target)
            } else {
                target
            }
        } else {
            target
        };

        let path = Path::new(target);
        if !path.exists() {
            anyhow::bail!("Telegram attachment path not found: {target}");
        }

        match attachment.kind {
            TelegramAttachmentKind::Image => self.send_photo(chat_id, thread_id, path, None).await,
            TelegramAttachmentKind::Document => {
                self.send_document(chat_id, thread_id, path, None).await
            }
            TelegramAttachmentKind::Video => self.send_video(chat_id, thread_id, path, None).await,
            TelegramAttachmentKind::Audio => self.send_audio(chat_id, thread_id, path, None).await,
            TelegramAttachmentKind::Voice => self.send_voice(chat_id, thread_id, path, None).await,
        }
    }

    /// Send a document/file to a Telegram chat
    pub async fn send_document(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendDocument failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "document sent to"
        );
        Ok(())
    }

    /// Send a document from bytes (in-memory) to a Telegram chat
    pub async fn send_document_bytes(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_bytes: Vec<u8>,
        file_name: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendDocument failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "document sent to"
        );
        Ok(())
    }

    /// Send a photo to a Telegram chat
    pub async fn send_photo(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("photo.jpg");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendPhoto"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendPhoto failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "photo sent to"
        );
        Ok(())
    }

    /// Send a photo from bytes (in-memory) to a Telegram chat
    pub async fn send_photo_bytes(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_bytes: Vec<u8>,
        file_name: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendPhoto"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendPhoto failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "photo sent to"
        );
        Ok(())
    }

    /// Send a video to a Telegram chat
    pub async fn send_video(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("video", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendVideo"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendVideo failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "video sent to"
        );
        Ok(())
    }

    /// Send an audio file to a Telegram chat
    pub async fn send_audio(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.mp3");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("audio", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendAudio"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendAudio failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "audio sent to"
        );
        Ok(())
    }

    /// Send a voice message to a Telegram chat
    pub async fn send_voice(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("voice.ogg");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("voice", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendVoice"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendVoice failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "file_name": file_name})),
            "voice sent to"
        );
        Ok(())
    }

    /// Send a file by URL (Telegram will download it)
    pub async fn send_document_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "document": url
        });

        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        if let Some(cap) = caption {
            body["caption"] = serde_json::Value::String(cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendDocument"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendDocument by URL failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "url": url})),
            "document (URL) sent to"
        );
        Ok(())
    }

    /// Send a photo by URL (Telegram will download it)
    pub async fn send_photo_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "photo": url
        });

        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        if let Some(cap) = caption {
            body["caption"] = serde_json::Value::String(cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendPhoto"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendPhoto by URL failed: {err}");
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"chat_id": chat_id, "url": url})),
            "photo (URL) sent to"
        );
        Ok(())
    }

    /// Send a video by URL (Telegram will download it)
    pub async fn send_video_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_media_by_url("sendVideo", "video", chat_id, thread_id, url, caption)
            .await
    }

    /// Send an audio file by URL (Telegram will download it)
    pub async fn send_audio_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_media_by_url("sendAudio", "audio", chat_id, thread_id, url, caption)
            .await
    }

    /// Send a voice message by URL (Telegram will download it)
    pub async fn send_voice_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_media_by_url("sendVoice", "voice", chat_id, thread_id, url, caption)
            .await
    }

    /// Fixed, bounded delay between retries of a transiently failing update.
    /// The attempt count is diagnostic only: only an explicitly permanent
    /// disposition may advance the Telegram offset.
    const TRANSIENT_RETRY_DELAY_SECS: u64 = 2;

    /// Every Nth failed attempt escalates the poison WARN to ERROR with
    /// operator guidance. Purely diagnostic — the retry posture itself
    /// never changes without an explicit operator skip.
    const POISON_ESCALATE_EVERY_ATTEMPTS: u32 = 15;

    /// Route a single update from a `getUpdates` batch through the shared
    /// delivered/permanent-skip/retry-transient disposition path.
    ///
    /// This is called from both the startup/restart probe and the main
    /// long-poll loop so a queued update sitting in the probe's first batch
    /// is handled identically to one seen mid-run: `offset` only advances
    /// past an update once it has been delivered (`tx.send` succeeded) or
    /// permanently skipped, never while a transient failure or a dropped
    /// `tx` receiver could still cause it to be lost.
    async fn process_update(
        &self,
        update: &serde_json::Value,
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
        offset: &mut i64,
        transient_retry: &mut Option<(i64, u32)>,
    ) -> UpdateOutcome {
        let uid = update.get("update_id").and_then(serde_json::Value::as_i64);

        // ── Handle callback_query (inline keyboard taps) ──
        if let Some(cb) = update.get("callback_query") {
            let cb_id = cb
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let cb_data = cb
                .get("data")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();

            if let Some(rest) = cb_data.strip_prefix("approval:")
                && let Some((approval_id, action)) = rest.rsplit_once(':')
            {
                let response = match action {
                    "approve" => Some(zeroclaw_api::channel::ChannelApprovalResponse::Approve),
                    "always" => Some(zeroclaw_api::channel::ChannelApprovalResponse::AlwaysApprove),
                    "deny" => Some(zeroclaw_api::channel::ChannelApprovalResponse::Deny),
                    other => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"other": other})),
                            "Unknown approval callback action"
                        );
                        None
                    }
                };

                if let Some(resp) = response
                    && let Some(sender) = self.pending_approvals.lock().await.remove(approval_id)
                {
                    let _ = sender.send(resp);
                }

                let answer_text = match action {
                    "approve" => "✅ Approved",
                    "always" => "✅✅ Always approved",
                    "deny" => "❌ Denied",
                    _ => "⚠️ Unknown action",
                };
                let answer_body = serde_json::json!({
                    "callback_query_id": cb_id,
                    "text": answer_text,
                });
                if let Err(e) = self
                    .http_client()
                    .post(self.api_url("answerCallbackQuery"))
                    .json(&answer_body)
                    .send()
                    .await
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                        "answerCallbackQuery failed"
                    );
                }
            }

            // A callback_query is terminal for inbound processing: there is
            // no message to deliver downstream. A failed spinner dismissal
            // must not hold up the offset, since retrying the update would
            // re-run the approval side effect that has already been applied.
            if let Some(uid) = uid {
                *offset = uid + 1;
            }
            return UpdateOutcome::Advanced;
        }

        // `parse_update_message` handles text messages and has no fallible
        // I/O, so its `None` always means "not applicable", fall through to
        // the voice parser next. The voice and attachment parsers can
        // additionally fail transiently on download/transcription I/O; a
        // transient failure must abort this update's processing entirely
        // (not fall through to the next parser) so the offset stays put and
        // the next poll retries it.
        let disposition = if let Some(m) = self.parse_update_message(update) {
            UpdateDisposition::Parsed(Box::new(m))
        } else {
            match self.try_parse_voice_message(update).await {
                UpdateDisposition::SkipPermanent => self.try_parse_attachment_message(update).await,
                other => other,
            }
        };

        let msg = match disposition {
            UpdateDisposition::Parsed(m) => m,
            UpdateDisposition::SkipPermanent => {
                Box::pin(self.handle_unauthorized_message(update)).await;
                if let Some(uid) = uid {
                    *offset = uid + 1;
                    *transient_retry = None;
                }
                return UpdateOutcome::Advanced;
            }
            UpdateDisposition::RetryTransient => {
                let attempts = if let Some(uid) = uid {
                    let attempts = match *transient_retry {
                        Some((tracked_uid, n)) if tracked_uid == uid => n.saturating_add(1),
                        _ => 1,
                    };
                    *transient_retry = Some((uid, attempts));
                    attempts
                } else {
                    1
                };
                if let Some(uid) = uid
                    && let Some(marker) = self.operator_skip_marker_for(uid)
                {
                    match self.archive_skipped_update(uid, &marker, update).await {
                        Ok(archive_path) => {
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "update_id": uid,
                                    "alias": self.alias,
                                    "reason": marker.reason,
                                    "archive": archive_path.display().to_string(),
                                })),
                                "operator skip applied: poisoned update archived, offset advanced, batch resumed"
                            );
                            *offset = uid + 1;
                            *transient_retry = None;
                            return UpdateOutcome::Advanced;
                        }
                        Err(err) => {
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "update_id": uid,
                                    "alias": self.alias,
                                    "err": err,
                                })),
                                "operator skip found but archiving failed; update stays head-of-line (never dropped)"
                            );
                        }
                    }
                }
                if attempts > 0 && attempts % Self::POISON_ESCALATE_EVERY_ATTEMPTS == 0 {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "update_id": uid,
                                "attempts": attempts,
                                "retry_delay_secs": Self::TRANSIENT_RETRY_DELAY_SECS,
                                "alias": self.alias,
                                "skip_command": format!(
                                    "zeroclaw telegram skip-update --alias {} --update-id {}",
                                    self.alias,
                                    uid.map(|v| v.to_string()).unwrap_or_default()
                                ),
                            })),
                        "poisoned update is head-of-line blocking this bot; run the skip command to archive and advance"
                    );
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "update_id": uid,
                                "attempts": attempts,
                                "retry_delay_secs": Self::TRANSIENT_RETRY_DELAY_SECS,
                            })),
                        "Transient failure parsing update; leaving offset unadvanced so the next poll retries it"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(
                    Self::TRANSIENT_RETRY_DELAY_SECS,
                ))
                .await;
                return UpdateOutcome::StopBatch;
            }
        };

        if self.ack_reactions
            && let Some((reaction_chat_id, reaction_message_id)) =
                Self::extract_update_message_target(update)
        {
            self.try_add_ack_reaction_nonblocking(reaction_chat_id, reaction_message_id);
        }

        let typing_body = serde_json::json!({
            "chat_id": &msg.reply_target,
            "action": "typing"
        });
        let _ = self
            .http_client()
            .post(self.api_url("sendChatAction"))
            .json(&typing_body)
            .send()
            .await;

        match tx.send(*msg).await {
            Ok(()) => {
                if let Some(uid) = uid {
                    *offset = uid + 1;
                    *transient_retry = None;
                }
                UpdateOutcome::Advanced
            }
            Err(_) => UpdateOutcome::ReceiverClosed,
        }
    }
}

impl ::zeroclaw_api::attribution::Attributable for TelegramChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(
            ::zeroclaw_api::attribution::ChannelKind::Telegram,
        )
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn self_handle(&self) -> Option<String> {
        self.bot_username.lock().clone()
    }

    /// Telegram users mention the bot as `@bot_username` in chat. The
    /// cached `bot_username` from `getMe` is already the bare form;
    /// prepend `@` to match what arrives in inbound message text.
    fn self_addressed_mention(&self) -> Option<String> {
        self.self_handle().map(|name| {
            let trimmed = name.trim_start_matches('@');
            format!("@{trimmed}")
        })
    }

    fn supports_draft_updates(&self) -> bool {
        self.stream_mode != StreamMode::Off
    }

    async fn send_draft(&self, message: &SendMessage) -> anyhow::Result<Option<String>> {
        if self.stream_mode == StreamMode::Off {
            return Ok(None);
        }

        let (chat_id, thread_id) = Self::parse_reply_target(&message.recipient);
        let initial_text = if message.content.is_empty() {
            "...".to_string()
        } else {
            message.content.clone()
        };

        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": initial_text,
        });
        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        let resp = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram sendMessage (draft) failed: {err}");
        }

        let resp_json: serde_json::Value = resp.json().await?;
        let message_id = resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|id| id.as_i64())
            .map(|id| id.to_string());

        self.last_draft_edit
            .lock()
            .insert(chat_id.to_string(), std::time::Instant::now());

        Ok(message_id)
    }

    async fn update_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let (chat_id, _) = Self::parse_reply_target(recipient);

        // Rate-limit edits per chat
        {
            let last_edits = self.last_draft_edit.lock();
            if let Some(last_time) = last_edits.get(&chat_id) {
                let elapsed = u64::try_from(last_time.elapsed().as_millis()).unwrap_or(u64::MAX);
                if elapsed < self.draft_update_interval_ms {
                    return Ok(());
                }
            }
        }

        // Truncate to Telegram limit for mid-stream edits (UTF-8 safe)
        let display_text = if text.len() > TELEGRAM_MAX_MESSAGE_LENGTH {
            let mut end = 0;
            for (idx, ch) in text.char_indices() {
                let next = idx + ch.len_utf8();
                if next > TELEGRAM_MAX_MESSAGE_LENGTH {
                    break;
                }
                end = next;
            }
            &text[..end]
        } else {
            text
        };

        let message_id_parsed = match message_id.parse::<i64>() {
            Ok(id) => id,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e)), "message_id": message_id})
                        ),
                    "Invalid Telegram message_id ''"
                );
                return Ok(());
            }
        };

        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id_parsed,
            "text": display_text,
        });

        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            self.last_draft_edit
                .lock()
                .insert(chat_id.clone(), std::time::Instant::now());
        } else {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"error": format!("{}", err), "status": status.to_string()})), "editMessageText failed");
        }

        Ok(())
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
        suppress_voice: bool,
    ) -> anyhow::Result<()> {
        let text = &strip_tool_call_tags(text);
        let (chat_id, thread_id) = Self::parse_reply_target(recipient);

        // Queue TTS voice reply — immediate mode since text is already final.
        // Skipped when suppress_voice is set (explicit text-only routing override).
        if !suppress_voice {
            self.try_queue_voice_reply(recipient, text, true, false);
        }

        // Clean up rate-limit tracking for this chat
        self.last_draft_edit.lock().remove(&chat_id);

        // Voice-only peers: delete the draft placeholder and let the voice
        // bubble be the sole reply. Bypassed when suppress_voice forces text.
        if !suppress_voice && self.is_voice_peer(recipient) {
            if let Ok(id) = message_id.parse::<i64>() {
                let _ = self
                    .client
                    .post(self.api_url("deleteMessage"))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": id,
                    }))
                    .send()
                    .await;
            }
            return Ok(());
        }

        // Parse attachments before processing
        let (text_without_markers, attachments) = parse_attachment_markers(text);

        // Parse message ID once for reuse
        let msg_id = match message_id.parse::<i64>() {
            Ok(id) => Some(id),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e)), "message_id": message_id})
                        ),
                    "Invalid Telegram message_id ''"
                );
                None
            }
        };

        // If we have attachments, delete the draft and send fresh messages
        // (Telegram editMessageText can't add attachments)
        if !attachments.is_empty() {
            // Delete the draft message
            if let Some(id) = msg_id {
                let _ = self
                    .client
                    .post(self.api_url("deleteMessage"))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": id,
                    }))
                    .send()
                    .await;
            }

            // Send text without markers
            if !text_without_markers.is_empty() {
                self.send_text_chunks(&text_without_markers, &chat_id, thread_id.as_deref())
                    .await?;
            }

            // Send attachments
            for attachment in &attachments {
                self.send_attachment(&chat_id, thread_id.as_deref(), attachment)
                    .await?;
            }

            return Ok(());
        }

        // If text exceeds limit, delete draft and send as chunked messages
        if text.len() > TELEGRAM_MAX_MESSAGE_LENGTH {
            if let Some(id) = msg_id {
                let _ = self
                    .client
                    .post(self.api_url("deleteMessage"))
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "message_id": id,
                    }))
                    .send()
                    .await;
            }

            // Fall back to chunked send
            return self
                .send_text_chunks(text, &chat_id, thread_id.as_deref())
                .await;
        }

        let Some(id) = msg_id else {
            return self
                .send_text_chunks(text, &chat_id, thread_id.as_deref())
                .await;
        };

        // Try editing with HTML formatting
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": id,
            "text": Self::markdown_to_telegram_html(text),
            "parse_mode": "HTML",
        });

        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;

        match Self::classify_edit_message_response(resp).await {
            EditMessageResult::Success | EditMessageResult::NotModified => return Ok(()),
            EditMessageResult::Failed(status) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"status": status.to_string()})),
                    "Telegram finalize_draft HTML edit failed; retrying without parse_mode"
                );
            }
        }

        // HTML failed — retry without parse_mode
        let plain_body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": id,
            "text": text,
        });

        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&plain_body)
            .send()
            .await?;

        match Self::classify_edit_message_response(resp).await {
            EditMessageResult::Success | EditMessageResult::NotModified => return Ok(()),
            EditMessageResult::Failed(status) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"status": status.to_string()})),
                    "Telegram finalize_draft plain edit failed; attempting delete+send fallback"
                );
            }
        }

        let delete_resp = self
            .client
            .post(self.api_url("deleteMessage"))
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": id,
            }))
            .send()
            .await;

        match delete_resp {
            Ok(resp) if resp.status().is_success() => {
                self.send_text_chunks(text, &chat_id, thread_id.as_deref())
                    .await
            }
            Ok(resp) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"status": resp.status().to_string()})),
                    "Telegram finalize_draft delete failed; skipping sendMessage to avoid duplicate"
                );
                Ok(())
            }
            Err(err) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"err": err.to_string()})),
                    "Telegram finalize_draft delete request failed: ; skipping sendMessage to avoid duplicate"
                );
                Ok(())
            }
        }
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
        let (chat_id, _) = Self::parse_reply_target(recipient);
        self.last_draft_edit.lock().remove(&chat_id);

        let message_id = match message_id.parse::<i64>() {
            Ok(id) => id,
            Err(e) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(
                            ::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e)), "message_id": message_id})
                        ),
                    "Invalid Telegram draft message_id ''"
                );
                return Ok(());
            }
        };

        let response = self
            .client
            .post(self.api_url("deleteMessage"))
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"status": status.to_string(), "body": body})),
                "deleteMessage failed"
            );
        }

        Ok(())
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Strip tool_call tags before processing to prevent Markdown parsing failures
        let content = strip_tool_call_tags(&message.content);

        // Parse recipient: "chat_id" or "chat_id:thread_id" format
        let (chat_id, thread_id) = match message.recipient.split_once(':') {
            Some((chat, thread)) => (chat, Some(thread)),
            None => (message.recipient.as_str(), None),
        };

        // Voice chat mode: queue a voice note. Suppressed messages (errors,
        // system notices) are never voiced.
        if !message.suppress_voice {
            self.try_queue_voice_reply(&message.recipient, &content, false, message.force_voice);
        }

        // Voice-only peers (or explicit force_voice): the voice note is the sole reply — skip text.
        if !message.suppress_voice
            && (self.is_voice_peer(&message.recipient) || message.force_voice)
        {
            return Ok(());
        }

        let (text_without_markers, attachments) = parse_attachment_markers(&content);

        if !attachments.is_empty() {
            if !text_without_markers.is_empty() {
                self.send_text_chunks(&text_without_markers, chat_id, thread_id)
                    .await?;
            }

            for attachment in &attachments {
                self.send_attachment(chat_id, thread_id, attachment).await?;
            }

            return Ok(());
        }

        if let Some(attachment) = parse_path_only_attachment(&content) {
            self.send_attachment(chat_id, thread_id, &attachment)
                .await?;
            return Ok(());
        }

        self.send_text_chunks(&content, chat_id, thread_id).await
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let mut offset: i64 = 0;
        // Single-slot transient-retry tracker: (update_id, attempts so far).
        // One slot is sufficient because a transient failure via
        // `process_update` stops processing of the current update batch, so
        // at most one update can be head-of-line blocking retries at any
        // time. The attempt count is diagnostic only.
        let mut transient_retry: Option<(i64, u32)> = None;

        if self.mention_only {
            let _ = self.get_bot_username().await;
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "channel listening for messages..."
        );

        loop {
            let url = self.api_url("getUpdates");
            let probe = serde_json::json!({
                "offset": offset,
                "timeout": 0,
                "allowed_updates": ["message", "callback_query"]
            });
            match self.http_client().post(&url).json(&probe).send().await {
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                ::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})
                            ),
                        "startup probe error; retrying in 5s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                Ok(resp) => {
                    match resp.json::<serde_json::Value>().await {
                        Err(e) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({"e": e.to_string()})),
                                "startup probe parse error: ; retrying in 5s"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Ok(data) => {
                            let ok = data
                                .get("ok")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false);
                            if ok {
                                // Slot claimed. Route any queued updates through the
                                // same delivered/permanent-skip/retry-transient
                                // disposition path as the main loop below.
                                if let Some(results) =
                                    data.get("result").and_then(serde_json::Value::as_array)
                                {
                                    for update in results {
                                        match self
                                            .process_update(
                                                update,
                                                &tx,
                                                &mut offset,
                                                &mut transient_retry,
                                            )
                                            .await
                                        {
                                            UpdateOutcome::Advanced => {}
                                            UpdateOutcome::StopBatch => break,
                                            UpdateOutcome::ReceiverClosed => return Ok(()),
                                        }
                                    }
                                }
                                break; // Probe succeeded; enter the long-poll loop.
                            }

                            let error_code = data
                                .get("error_code")
                                .and_then(serde_json::Value::as_i64)
                                .unwrap_or_default();
                            if error_code == 409 {
                                ::zeroclaw_log::record!(
                                    DEBUG,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    ),
                                    "Startup probe: slot busy (409), retrying in 5s"
                                );
                            } else {
                                let desc = data
                                    .get("description")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("unknown");
                                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"error_code": error_code, "desc": desc})), "Startup probe: API error : ; retrying in 5s");
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        }

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Startup probe succeeded; entering main long-poll loop."
        );

        self.register_bot_commands().await;

        loop {
            if self.mention_only {
                let missing_username = self.bot_username.lock().is_none();
                if missing_username {
                    let _ = self.get_bot_username().await;
                }
            }

            let url = self.api_url("getUpdates");
            let body = serde_json::json!({
                "offset": offset,
                "timeout": 30,
                "allowed_updates": ["message", "callback_query"]
            });

            let resp = match self.http_client().post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                ::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})
                            ),
                        "poll error"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                ::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})
                            ),
                        "parse error"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let ok = data
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            if !ok {
                let error_code = data
                    .get("error_code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default();
                let description = data
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown Telegram API error");

                if error_code == 409 {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"description": description})),
                        "Telegram polling conflict (409): . \
Ensure only one `zeroclaw` process is using this bot token."
                    );
                    // Back off for 35 seconds — longer than Telegram's 30-second poll
                    // timeout — so any competing session (e.g. a stale connection from
                    // a previous daemon) has time to expire before we retry.
                    tokio::time::sleep(std::time::Duration::from_secs(35)).await;
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        &format!(
                            "Telegram getUpdates API error (code={}): {description}",
                            error_code
                        )
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                continue;
            }

            if let Some(results) = data.get("result").and_then(serde_json::Value::as_array) {
                for update in results {
                    match self
                        .process_update(update, &tx, &mut offset, &mut transient_retry)
                        .await
                    {
                        UpdateOutcome::Advanced => {}
                        UpdateOutcome::StopBatch => break,
                        UpdateOutcome::ReceiverClosed => return Ok(()),
                    }
                }
            }
        }
    }

    async fn health_check(&self) -> bool {
        let timeout_duration = Duration::from_secs(5);

        match tokio::time::timeout(
            timeout_duration,
            self.http_client().get(self.api_url("getMe")).send(),
        )
        .await
        {
            Ok(Ok(resp)) => resp.status().is_success(),
            Ok(Err(e)) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"error": zeroclaw_runtime::security::scrub(&format!("{}", e))})),
                    "health check failed"
                );
                false
            }
            Err(_) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "health check timed out after 5s"
                );
                false
            }
        }
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.stop_typing(recipient).await?;

        let client = self.http_client();
        let url = self.api_url("sendChatAction");
        let chat_id = recipient.to_string();

        let handle = zeroclaw_spawn::spawn!(async move {
            loop {
                let body = serde_json::json!({
                    "chat_id": &chat_id,
                    "action": "typing"
                });
                let _ = client.post(&url).json(&body).send().await;
                // Telegram typing indicator expires after 5s; refresh at 4s
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });

        let mut guard = self.typing_handle.lock();
        *guard = Some(handle);

        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        let mut guard = self.typing_handle.lock();
        if let Some(handle) = guard.take() {
            handle.abort();
        }
        Ok(())
    }

    /// Delegates to [`Self::request_approval_attributed`] and drops the
    /// provenance, so the prompt/timeout logic lives in exactly one place.
    async fn request_approval(
        &self,
        recipient: &str,
        request: &zeroclaw_api::channel::ChannelApprovalRequest,
    ) -> anyhow::Result<Option<zeroclaw_api::channel::ChannelApprovalResponse>> {
        Ok(self
            .request_approval_attributed(recipient, request)
            .await?
            .map(|attributed| attributed.response))
    }

    async fn request_approval_attributed(
        &self,
        recipient: &str,
        request: &zeroclaw_api::channel::ChannelApprovalRequest,
    ) -> anyhow::Result<Option<zeroclaw_api::channel::AttributedApprovalResponse>> {
        use zeroclaw_api::channel::ChannelApprovalResponse;

        // Parse recipient for chat_id + optional thread_id ("chat_id:thread_id" format).
        let (chat_id, thread_id) = recipient
            .split_once(':')
            .map_or((recipient, None), |(c, t)| (c, Some(t)));

        // Unique key embedded in callback_data so listen() can route the tap.
        let approval_id = uuid::Uuid::new_v4().to_string();

        let tool = Self::escape_html(&request.tool_name);
        let args = Self::escape_html(&request.arguments_summary);
        let text = format!(
            "\u{1f527} <b>Tool approval required</b>\n\n\
             Tool: <code>{tool}</code>\n\
             {args}\n\n\
             Tap a button below:",
        );

        let reply_markup = serde_json::json!({
            "inline_keyboard": [[
                { "text": "✅ Approve",  "callback_data": format!("approval:{}:approve", approval_id) },
                { "text": "❌ Deny",     "callback_data": format!("approval:{}:deny", approval_id) },
                { "text": "✅✅ Always", "callback_data": format!("approval:{}:always", approval_id) },
            ]]
        });

        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "reply_markup": reply_markup,
        });
        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        // Register the oneshot BEFORE sending the message to avoid a race
        // where the user taps the button before the sender is in the map.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_approvals
            .lock()
            .await
            .insert(approval_id.clone(), tx);

        let resp = self
            .http_client()
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await;

        let send_ok = match resp {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => {
                let status = r.status();
                let err = r.text().await.unwrap_or_default();
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"status": status.to_string(), "err": err})
                        ),
                    "Telegram sendMessage (approval) with HTML failed; retrying without parse_mode"
                );

                // Fallback: plain text, no parse_mode, keep the buttons
                let plain_text = format!(
                    "🔧 Tool approval required\n\nTool: {}\n{}\n\nTap a button below:",
                    request.tool_name, request.arguments_summary
                );
                let mut plain_body = serde_json::json!({
                    "chat_id": chat_id,
                    "text": plain_text,
                    "reply_markup": reply_markup,
                });
                if let Some(tid) = thread_id {
                    plain_body["message_thread_id"] = serde_json::Value::String(tid.to_string());
                }

                let plain_resp = self
                    .http_client()
                    .post(self.api_url("sendMessage"))
                    .json(&plain_body)
                    .send()
                    .await;

                match plain_resp {
                    Ok(r) if r.status().is_success() => true,
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        self.pending_approvals.lock().await.remove(&approval_id);
                        anyhow::bail!("Telegram sendMessage (approval) failed ({status}): {err}");
                    }
                    Err(e) => {
                        self.pending_approvals.lock().await.remove(&approval_id);
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                self.pending_approvals.lock().await.remove(&approval_id);
                return Err(e.into());
            }
        };

        if !send_ok {
            self.pending_approvals.lock().await.remove(&approval_id);
            anyhow::bail!("Telegram sendMessage (approval) failed after fallback");
        }

        // Wait for the user to tap a button. Timeout is configurable via
        // `channels.telegram.approval_timeout_secs` (default 120s).
        let result =
            match tokio::time::timeout(Duration::from_secs(self.approval_timeout_secs), rx).await {
                Ok(Ok(response)) => Some(
                    zeroclaw_api::channel::AttributedApprovalResponse::operator(response),
                ),
                Ok(Err(_)) => {
                    // Sender dropped — clean up and deny. Nobody tapped.
                    self.pending_approvals.lock().await.remove(&approval_id);
                    Some(
                        zeroclaw_api::channel::AttributedApprovalResponse::from_runtime(
                            ChannelApprovalResponse::Deny,
                            zeroclaw_api::channel::ApprovalSource::Unreachable,
                        ),
                    )
                }
                Err(_) => {
                    // Timeout — clean up and deny. This is the runtime's deny,
                    // not the operator's.
                    self.pending_approvals.lock().await.remove(&approval_id);
                    Some(
                        zeroclaw_api::channel::AttributedApprovalResponse::from_runtime(
                            ChannelApprovalResponse::Deny,
                            zeroclaw_api::channel::ApprovalSource::TimedOut,
                        ),
                    )
                }
            };

        Ok(result)
    }
}

#[cfg(test)]
mod tests;
