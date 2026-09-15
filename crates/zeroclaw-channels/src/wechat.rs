//! WeChat personal iLink Bot channel.

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, block_padding::Pkcs7};
use anyhow::Context;
use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_config::paths::{normalize_lexical, resolve_under};
use zeroclaw_config::schema::Config;
use zeroclaw_runtime::i18n;
use zeroclaw_runtime::security::pairing::PairingGuard;

const DEFAULT_API_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// Long-poll timeout for getUpdates (server may hold the request up to this).
const LONG_POLL_TIMEOUT_MS: u64 = 35_000;
/// Regular API request timeout.
const API_TIMEOUT: Duration = Duration::from_secs(15);

/// Session-expired error code returned by the iLink API.
const SESSION_EXPIRED_ERRCODE: i64 = -14;
/// Pause duration after session expiry before retrying.
const SESSION_PAUSE_DURATION: Duration = Duration::from_secs(60 * 60);
/// Maximum consecutive API failures before backing off.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Back-off delay after reaching max consecutive failures.
const BACKOFF_DELAY: Duration = Duration::from_secs(30);
/// Retry delay for a single failure.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// QR code long-poll timeout.
const QR_POLL_TIMEOUT: Duration = Duration::from_secs(35);
/// Maximum QR code refresh attempts.
const MAX_QR_REFRESH: u32 = 3;
/// Total QR scan wait timeout.
const QR_SCAN_TIMEOUT: Duration = Duration::from_secs(480);

const WECHAT_BIND_COMMAND: &str = "/bind";

/// State-dir file holding the persisted bot token / account identity.
/// Single source of truth for every reader, writer, and the relink purge.
const ACCOUNT_FILE: &str = "account.json";
/// State-dir file holding the persisted sync cursor and context tokens.
const SYNC_FILE: &str = "sync.json";

/// iLink Bot message types.
const MESSAGE_TYPE_BOT: u32 = 2;
/// iLink Bot message state.
const MESSAGE_STATE_FINISH: u32 = 2;
/// iLink Bot message item type: text.
const ITEM_TYPE_TEXT: u32 = 1;
/// iLink Bot message item type: image.
const ITEM_TYPE_IMAGE: u32 = 2;
/// iLink Bot message item type: voice.
const ITEM_TYPE_VOICE: u32 = 3;
/// iLink Bot message item type: file.
const ITEM_TYPE_FILE: u32 = 4;
/// iLink Bot message item type: video.
const ITEM_TYPE_VIDEO: u32 = 5;

/// getUploadUrl media type: image.
const UPLOAD_MEDIA_TYPE_IMAGE: u32 = 1;
/// getUploadUrl media type: video.
const UPLOAD_MEDIA_TYPE_VIDEO: u32 = 2;
/// getUploadUrl media type: file/document.
const UPLOAD_MEDIA_TYPE_FILE: u32 = 3;

/// Shared max size for inbound/outbound media handling.
const WECHAT_MEDIA_MAX_BYTES: u64 = 100 * 1024 * 1024;

type Aes128EcbEnc = ecb::Encryptor<aes::Aes128>;
type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;

fn long_poll_client_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms + 5_000)
}

fn wechat_cli_string(key: &str) -> String {
    i18n::get_required_cli_string(key)
}

fn wechat_cli_string_with_args(key: &str, args: &[(&str, &str)]) -> String {
    i18n::get_required_cli_string_with_args(key, args)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WeChatAttachmentKind {
    Image,
    Document,
    Video,
    Audio,
    Voice,
}

impl WeChatAttachmentKind {
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

    fn default_extension(self) -> &'static str {
        match self {
            Self::Image => "png",
            Self::Document => "bin",
            Self::Video => "mp4",
            Self::Audio => "mp3",
            Self::Voice => "silk",
        }
    }

    fn upload_media_type(self) -> u32 {
        match self {
            Self::Image => UPLOAD_MEDIA_TYPE_IMAGE,
            Self::Video => UPLOAD_MEDIA_TYPE_VIDEO,
            Self::Document | Self::Audio | Self::Voice => UPLOAD_MEDIA_TYPE_FILE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WeChatAttachment {
    kind: WeChatAttachmentKind,
    target: String,
}

#[derive(Debug, Clone)]
struct WeChatMediaPayload {
    bytes: Vec<u8>,
    file_name: String,
}

#[derive(Debug, Clone)]
struct InboundAttachmentSpec {
    kind: WeChatAttachmentKind,
    encrypted_query_param: String,
    aes_key: Option<String>,
    file_name: String,
}

/// Why an inbound attachment download or save failed, classified so the
/// listener can hold the batch cursor on a transient error and skip only
/// when retrying cannot change the outcome.
///
/// Classification is deliberately conservative: only an explicit permanent
/// whitelist is terminal. The HTTP status classifier follows Telegram's
/// permanent-whitelist pattern (`FileLookupFailure`), but mixed-message
/// delivery is a WeChat product choice (keep the text) rather than
/// Telegram's drop-the-update rule.
/// Permanence requires HTTP `{400, 403, 404, 410}`, a size-limit rejection,
/// or invalid AES key metadata (wrong length/format, decidable before
/// touching the payload). 408, 425, 429, 5xx, transport errors, body-read
/// failures, PKCS7 decrypt failures on a downloaded payload, directory-create
/// failures, and workspace write failures stay `Transient`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentFailureKind {
    Transient,
    Permanent,
}

/// An inbound attachment failure with the classification preserved.
#[derive(Debug)]
struct AttachmentBuildFailure {
    kind: AttachmentFailureKind,
    message: String,
}

impl std::fmt::Display for AttachmentBuildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl AttachmentBuildFailure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: AttachmentFailureKind::Transient,
            message: message.into(),
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            kind: AttachmentFailureKind::Permanent,
            message: message.into(),
        }
    }

    fn kind(&self) -> AttachmentFailureKind {
        self.kind
    }

    /// Map a CDN HTTP status onto a classified failure.
    ///
    /// Permanent is a whitelist, not "all 4xx minus a denylist": a retryable
    /// code such as 408 or 425 must not silently drop a pure-attachment
    /// message by advancing the cursor.
    fn classify_status(status: reqwest::StatusCode, body: &str) -> Self {
        const PERMANENT_STATUSES: [u16; 4] = [400, 403, 404, 410];
        let message = format!("attachment download failed ({status}): {body}");
        if PERMANENT_STATUSES.contains(&status.as_u16()) {
            Self::permanent(message)
        } else {
            Self::transient(message)
        }
    }
}

/// Outcome of building inbound attachment content for one iLink message.
///
/// `None` means the message has no fetchable attachment (or no workspace
/// is configured). `SkipPermanent` is a logged, terminal skip of the
/// attachment itself: mixed text+media still delivers the text with an
/// unavailable notice, and a pure-attachment message is dropped.
/// `RetryTransient` stops the current batch so the cursor stays put and
/// the next poll retries the download.
#[derive(Debug)]
enum AttachmentDisposition {
    None,
    Ready(String),
    SkipPermanent(String),
    RetryTransient,
}

impl AttachmentDisposition {
    fn from_failure(failure: AttachmentBuildFailure, kind: WeChatAttachmentKind) -> Self {
        let (log_msg, disposition) = match failure.kind() {
            AttachmentFailureKind::Permanent => (
                "attachment permanently skipped",
                Self::SkipPermanent(format_unavailable_attachment(kind)),
            ),
            AttachmentFailureKind::Transient => (
                "transient attachment failure, batch will retry",
                Self::RetryTransient,
            ),
        };
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "error": failure.to_string(),
                    "classification": format!("{:?}", failure.kind()),
                })),
            log_msg
        );
        disposition
    }
}

/// Staged inbound work for one getUpdates batch. Items are published only
/// after the whole batch resolves without a transient attachment failure,
/// so a later retry does not redeliver already-sent messages.
enum StagedInbound {
    Deliver(Box<ChannelMessage>),
    Unauthorized { from_user_id: String, text: String },
}

/// Result of staging one getUpdates batch before any `tx.send`.
enum BatchOutcome {
    Ready(Vec<StagedInbound>),
    StopBatch,
}

#[derive(Debug, Clone)]
struct UploadedWeChatMedia {
    encrypted_query_param: String,
    aes_key_base64: String,
    raw_size: usize,
    encrypted_size: usize,
}

fn is_remote_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

fn infer_attachment_kind_from_target(target: &str) -> Option<WeChatAttachmentKind> {
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
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(WeChatAttachmentKind::Image),
        "mp4" | "mov" | "mkv" | "avi" | "webm" => Some(WeChatAttachmentKind::Video),
        "mp3" | "m4a" | "wav" | "flac" => Some(WeChatAttachmentKind::Audio),
        "ogg" | "oga" | "opus" | "silk" => Some(WeChatAttachmentKind::Voice),
        "pdf" | "txt" | "md" | "csv" | "json" | "zip" | "tar" | "gz" | "doc" | "docx" | "xls"
        | "xlsx" | "ppt" | "pptx" => Some(WeChatAttachmentKind::Document),
        _ => None,
    }
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

fn parse_attachment_markers(message: &str) -> (String, Vec<WeChatAttachment>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut attachments = Vec::new();
    let mut cursor = 0usize;

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
            let kind = WeChatAttachmentKind::from_marker(kind)?;
            let target = target.trim();
            if target.is_empty() {
                return None;
            }
            Some(WeChatAttachment {
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

fn parse_path_only_attachment(message: &str) -> Option<WeChatAttachment> {
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

    if !is_remote_url(candidate) && !Path::new(candidate).exists() {
        return None;
    }

    Some(WeChatAttachment {
        kind,
        target: candidate.to_string(),
    })
}

fn format_attachment_content(
    kind: WeChatAttachmentKind,
    local_filename: &str,
    local_path: &Path,
) -> String {
    if kind == WeChatAttachmentKind::Image {
        format!("[IMAGE:{}]", local_path.display())
    } else {
        format!("[Document: {}] {}", local_filename, local_path.display())
    }
}

/// Human-readable notice when a permanently skipped attachment still has
/// accompanying text. Matches the inbound status-marker shape used for
/// transcription failure (`[Audio: transcription failed]`) and media
/// pipeline annotations (`[Image: {name} attached]`), not the machine
/// `[IMAGE:/path]` payload marker.
fn format_unavailable_attachment(kind: WeChatAttachmentKind) -> String {
    match kind {
        WeChatAttachmentKind::Image => "[Image: unavailable]".to_string(),
        WeChatAttachmentKind::Document => "[Document: unavailable]".to_string(),
        WeChatAttachmentKind::Video => "[Video: unavailable]".to_string(),
        WeChatAttachmentKind::Audio => "[Audio: unavailable]".to_string(),
        WeChatAttachmentKind::Voice => "[Voice: unavailable]".to_string(),
    }
}

fn sanitize_attachment_filename(file_name: &str) -> Option<String> {
    let cleaned = Path::new(file_name)
        .file_name()
        .and_then(|name| name.to_str())?
        .trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned.to_string())
}

fn aes_ecb_padded_size(plaintext_size: usize) -> usize {
    ((plaintext_size / 16) + 1) * 16
}

fn encrypt_aes_ecb(plaintext: &[u8], key: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    let padded_size = aes_ecb_padded_size(plaintext.len());
    let mut buffer = vec![0u8; padded_size];
    buffer[..plaintext.len()].copy_from_slice(plaintext);
    let encrypted = Aes128EcbEnc::new(&(*key).into())
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
        .map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "media encrypt failed"
            );
            anyhow::Error::msg(format!("media encrypt failed: {e}"))
        })?;
    Ok(encrypted.to_vec())
}

fn decrypt_aes_ecb(ciphertext: &[u8], key: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    let mut buffer = ciphertext.to_vec();
    Aes128EcbDec::new(&(*key).into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map(|decrypted| decrypted.to_vec())
        .map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "wechat: media decrypt failed"
            );
            anyhow::Error::msg(format!("media decrypt failed: {e}"))
        })
}

fn parse_aes_key(raw: &str) -> anyhow::Result<[u8; 16]> {
    let raw = raw.trim();
    if raw.len() == 32 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        let bytes = hex::decode(raw).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "media hex aes_key invalid"
            );
            anyhow::Error::msg(format!("media hex aes_key invalid: {e}"))
        })?;
        return <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"key_kind": "hex", "expected_bytes": 16})),
                "wechat: media hex aes_key has wrong byte length"
            );
            anyhow::Error::msg("media hex aes_key must be 16 bytes")
        });
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "media base64 aes_key invalid"
            );
            anyhow::Error::msg(format!("media base64 aes_key invalid: {e}"))
        })?;

    if decoded.len() == 16 {
        return <[u8; 16]>::try_from(decoded.as_slice()).map_err(|_| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"key_kind": "base64", "expected_bytes": 16})),
                "wechat: media base64 aes_key has wrong byte length"
            );
            anyhow::Error::msg("media base64 aes_key must be 16 bytes")
        });
    }

    if decoded.len() == 32 && decoded.iter().all(u8::is_ascii_hexdigit) {
        let hex_text = std::str::from_utf8(&decoded).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "media aes_key utf8 invalid"
            );
            anyhow::Error::msg(format!("media aes_key utf8 invalid: {e}"))
        })?;
        let bytes = hex::decode(hex_text).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "media nested hex aes_key invalid"
            );
            anyhow::Error::msg(format!("media nested hex aes_key invalid: {e}"))
        })?;
        return <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(
                        ::serde_json::json!({"key_kind": "nested_hex", "expected_bytes": 16})
                    ),
                "wechat: media nested hex aes_key has wrong byte length"
            );
            anyhow::Error::msg("media nested hex aes_key must be 16 bytes")
        });
    }

    anyhow::bail!(
        "media aes_key must decode to 16 raw bytes or 32 hex chars, got {} bytes",
        decoded.len()
    )
}

fn https_base_url(
    field_name: &str,
    value: Option<String>,
    default: &str,
) -> anyhow::Result<String> {
    let url = value.unwrap_or_else(|| default.to_string());
    let url = url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("https://") {
        anyhow::bail!("{field_name} must use https://, got {url}");
    }
    Ok(url)
}

/// Interpret an iLink `sendmessage` response body, returning a description
/// of the failure when the API reported one.
///
/// The iLink API reports send failures as HTTP 200 with a non-zero
/// `ret`/`errcode` in the JSON body — the same envelope the getUpdates
/// sync loop parses. Checking only the HTTP status treats those failures
/// (e.g. an expired or missing `context_token`) as success, so the message
/// is silently dropped.
///
/// An empty or non-JSON 2xx body carries no envelope to inspect and is
/// treated as success, preserving the pre-check behavior for those shapes.
fn sendmessage_body_error(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }
    let Ok(data) = serde_json::from_str::<serde_json::Value>(body) else {
        return None;
    };
    let ret = data.get("ret").and_then(|v| v.as_i64()).unwrap_or(0);
    let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(0);
    if ret == 0 && errcode == 0 {
        return None;
    }
    let errmsg = data.get("errmsg").and_then(|v| v.as_str()).unwrap_or("");
    Some(format!("ret={ret}, errcode={errcode}, errmsg={errmsg:?}"))
}

/// WeChat iLink Bot channel — long-polls the iLink Bot API for updates.
pub struct WeChatChannel {
    /// Bot token obtained via QR-code login; `None` until first login.
    bot_token: RwLock<Option<String>>,
    /// iLink bot ID (account ID); set after QR login.
    account_id: RwLock<Option<String>>,
    /// API base URL.
    api_base_url: String,
    /// CDN base URL.
    cdn_base_url: String,
    /// The alias key under `[channels.wechat.<alias>]` this handle is
    /// bound to. Used to scope peer-group writes and resolver lookups.
    alias: String,
    /// Resolves inbound external peers from canonical state at message-time.
    /// No cache (see AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH").
    peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    persist: Option<Arc<parking_lot::RwLock<Config>>>,
    /// Pairing guard for /bind flow.
    pairing: Option<PairingGuard>,
    /// HTTP client for API requests.
    client: reqwest::Client,
    /// Per-user context_token cache (accountId:userId -> token).
    context_tokens: Mutex<HashMap<String, String>>,
    /// Per-user typing_ticket cache (userId -> ticket).
    typing_tickets: Mutex<HashMap<String, String>>,
    /// Persisted getUpdates cursor.
    cursor: Mutex<String>,
    /// Typing indicator task handle.
    typing_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// State directory for persisting token & cursor.
    state_dir: PathBuf,
    /// Workspace directory used for storing inbound attachments and resolving
    /// `/workspace/...` paths from generated replies.
    workspace_dir: Option<PathBuf>,
}

/// Persistent account data (token + metadata).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AccountData {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    saved_at: Option<String>,
}

/// Persistent sync cursor and context tokens.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SyncData {
    #[serde(default)]
    get_updates_buf: String,
    #[serde(default)]
    context_tokens: HashMap<String, String>,
}

/// Write bytes to `path` via a sibling temp file: `create_new`, write,
/// `sync_all`, chmod 0o600 (Unix), then `rename` over the destination.
///
/// Process-crash safety: the previous durable file is never truncated in
/// place, so a crash mid-write cannot replace it with a torn file. Power-loss
/// durability is best-effort (`sync_all` on the temp file; Unix parent-dir
/// fsync after rename, logged on failure) — not a guarantee.
///
/// Windows `std::fs::rename` is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
/// (see the Rust std docs for [`std::fs::rename`]), so a successful rename
/// replaces an existing destination. Residual risk: if another handle has the
/// destination open without `FILE_SHARE_DELETE`, the replace can still fail.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp = next_private_tmp(dir, file_name, &SEQ);
    match write_private_to_tmp(&tmp, path, data) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Same-PID / other-namespace collision on the tmp name: retry
            // once with a fresh name. Do not unlink the colliding file —
            // we did not create it.
            let tmp = next_private_tmp(dir, file_name, &SEQ);
            write_private_to_tmp(&tmp, path, data)
        }
        other => other,
    }
}

fn next_private_tmp(dir: &Path, file_name: &str, seq: &AtomicU64) -> PathBuf {
    dir.join(format!(
        ".{}.{}.{}.tmp",
        file_name,
        std::process::id(),
        seq.fetch_add(1, Ordering::Relaxed)
    ))
}

fn write_private_to_tmp(tmp: &Path, dest: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let created: std::io::Result<()> = (|| {
        let mut file = std::fs::File::create_new(tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, dest)?;
        Ok(())
    })();
    if let Err(e) = &created {
        // Only unlink a tmp we created. `AlreadyExists` from `create_new`
        // means another writer owns that name.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            let _ = std::fs::remove_file(tmp);
        }
    }
    created?;

    #[cfg(unix)]
    if let Err(e) = sync_parent_dir(dest) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "best-effort parent directory fsync failed after WeChat state rename"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file = std::fs::File::open(dir)?;
    file.sync_all()
}

fn persist_sync_data(
    state_dir: &Path,
    cursor: String,
    context_tokens: HashMap<String, String>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir).context("failed to create state dir")?;
    let data = SyncData {
        get_updates_buf: cursor,
        context_tokens,
    };
    let json = serde_json::to_string(&data).context("failed to serialize sync data")?;
    write_private(&state_dir.join(SYNC_FILE), json.as_bytes()).context("failed to write sync data")
}

/// Generate a random X-WECHAT-UIN header value.
fn random_wechat_uin() -> String {
    let bytes: [u8; 4] = rand::random();
    let uint32 = u32::from_be_bytes(bytes);
    base64::engine::general_purpose::STANDARD.encode(uint32.to_string())
}

fn build_base_info() -> serde_json::Value {
    serde_json::json!({
        "channel_version": env!("CARGO_PKG_VERSION")
    })
}

static CODE_BLOCK_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"```[^\n]*\n?([\s\S]*?)```").unwrap());
static IMAGE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"!\[[^\]]*\]\([^)]*\)").unwrap());
static LINK_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[([^\]]+)\]\([^)]*\)").unwrap());
static HEADING_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^\s{0,3}#{1,6}\s+").unwrap());
static BLOCKQUOTE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^>\s?").unwrap());
static BULLET_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?m)^\s*[-*+]\s+").unwrap());
static EMPHASIS_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\*\*|__|~~|`|\*)").unwrap());
static TABLE_SEPARATOR_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\|[\s:|-]+\|$").unwrap());
static TABLE_ROW_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\|(.+)\|$").unwrap());

fn markdown_to_plain_text(text: &str) -> String {
    let mut result = CODE_BLOCK_RE.replace_all(text, "$1").into_owned();
    result = IMAGE_RE.replace_all(&result, "").into_owned();
    result = LINK_RE.replace_all(&result, "$1").into_owned();

    let mut lines = Vec::new();
    for line in result.lines() {
        if TABLE_SEPARATOR_RE.is_match(line) {
            continue;
        }

        if let Some(captures) = TABLE_ROW_RE.captures(line) {
            let inner = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            lines.push(
                inner
                    .split('|')
                    .map(str::trim)
                    .filter(|cell| !cell.is_empty())
                    .collect::<Vec<_>>()
                    .join("  "),
            );
        } else {
            lines.push(line.to_string());
        }
    }

    result = lines.join("\n");
    result = HEADING_RE.replace_all(&result, "").into_owned();
    result = BLOCKQUOTE_RE.replace_all(&result, "").into_owned();
    result = BULLET_RE.replace_all(&result, "").into_owned();
    result = EMPHASIS_RE.replace_all(&result, "").into_owned();

    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    result.trim().to_string()
}

fn render_login_qr(code: &str) -> anyhow::Result<String> {
    let payload = code.trim();
    if payload.is_empty() {
        anyhow::bail!("QR payload is empty");
    }

    let qr = qrcode::QrCode::new(payload.as_bytes()).map_err(|err| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"error": format!("{}", err)})),
            "Failed to encode WeChat QR payload"
        );
        anyhow::Error::msg(format!("Failed to encode WeChat QR payload: {err}"))
    })?;

    Ok(qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

/// Build common request headers for iLink API.
fn build_headers(token: Option<&str>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers.insert("AuthorizationType", "ilink_bot_token".parse().unwrap());
    headers.insert("X-WECHAT-UIN", random_wechat_uin().parse().unwrap());
    if let Some(t) = token
        && !t.is_empty()
        && let Ok(val) = format!("Bearer {t}").parse()
    {
        headers.insert("Authorization", val);
    }
    headers
}

/// Extract text content from an iLink message's item_list.
fn extract_text_from_items(items: &[serde_json::Value]) -> String {
    for item in items {
        let item_type = item
            .get("type")
            .and_then(|v| v.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        match item_type {
            ITEM_TYPE_TEXT => {
                if let Some(text) = item
                    .get("text_item")
                    .and_then(|ti| ti.get("text"))
                    .and_then(|t| t.as_str())
                {
                    // Handle ref_msg (quoted message)
                    let ref_prefix = if let Some(ref_msg) = item.get("ref_msg") {
                        let title = ref_msg.get("title").and_then(|t| t.as_str()).unwrap_or("");
                        if title.is_empty() {
                            String::new()
                        } else {
                            format!("[引用: {title}]\n")
                        }
                    } else {
                        String::new()
                    };
                    return format!("{ref_prefix}{text}");
                }
            }
            ITEM_TYPE_VOICE => {
                // Voice-to-text transcription
                if let Some(text) = item
                    .get("voice_item")
                    .and_then(|vi| vi.get("text"))
                    .and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    return text.to_string();
                }
            }
            _ => {}
        }
    }
    String::new()
}

impl WeChatChannel {
    pub fn new(
        alias: impl Into<String>,
        peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        api_base_url: Option<String>,
        cdn_base_url: Option<String>,
        state_dir: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let api_base_url = https_base_url("api_base_url", api_base_url, DEFAULT_API_BASE_URL)?;
        let cdn_base_url = https_base_url("cdn_base_url", cdn_base_url, CDN_BASE_URL)?;

        let alias = alias.into();
        let has_peers = !peer_resolver().is_empty();
        let pairing = if has_peers {
            None
        } else {
            let guard = PairingGuard::new(true, &[]);
            if let Some(code) = guard.pairing_code() {
                // Mirror Telegram: a backgrounded daemon discards stdout, so
                // also record the one-time bind code through the structured
                // log where `zeroclaw service logs` / the gateway can find it.
                // Tag it `Channel` so the web Logs page shows it by default
                // (an untagged event defaults to `Internal` and is hidden).
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_attrs(::serde_json::json!({
                            "alias": alias.as_str(),
                            "pairing_code": code.as_str(),
                        })),
                    "WeChat pairing required; one-time bind code issued"
                );
                println!(
                    "  {}",
                    wechat_cli_string_with_args("cli-wechat-pairing-required", &[("code", &code)],)
                );
                println!(
                    "     {}",
                    wechat_cli_string_with_args(
                        "cli-wechat-send-bind-command",
                        &[("command", WECHAT_BIND_COMMAND)],
                    )
                );
            }
            Some(guard)
        };

        let state_dir = state_dir.unwrap_or_else(Self::default_state_dir);

        let mut channel = Self {
            bot_token: RwLock::new(None),
            account_id: RwLock::new(None),
            api_base_url,
            cdn_base_url,
            alias,
            peer_resolver,
            persist: None,
            pairing,
            client: reqwest::Client::new(),
            context_tokens: Mutex::new(HashMap::new()),
            typing_tickets: Mutex::new(HashMap::new()),
            cursor: Mutex::new(String::new()),
            typing_handle: Mutex::new(None),
            state_dir,
            workspace_dir: None,
        };

        // Try to load persisted state
        channel.load_persisted_state();
        Ok(channel)
    }

    pub fn with_workspace_dir(mut self, dir: PathBuf) -> Self {
        self.workspace_dir = Some(dir);
        self
    }

    /// Wire the shared Config handle so `persist_allowed_identity` can
    /// write a paired user into `peer_groups` and save. The long-running
    /// daemon sets this from the orchestrator; tests and one-shot
    /// callers leave it unset (pairing works at runtime, doesn't persist).
    pub fn with_persistence(mut self, config: Arc<parking_lot::RwLock<Config>>) -> Self {
        self.persist = Some(config);
        self
    }

    /// Default state directory when `[channels.wechat.<alias>] state_dir`
    /// is unset: `~/.zeroclaw/wechat`.
    fn default_state_dir() -> PathBuf {
        directories::UserDirs::new()
            .map(|u| u.home_dir().join(".zeroclaw").join("wechat"))
            .unwrap_or_else(|| PathBuf::from(".zeroclaw/wechat"))
    }

    /// Resolve the effective state directory from the raw
    /// `[channels.wechat.<alias>] state_dir` config value: tilde-expanded
    /// when set, [`Self::default_state_dir`] otherwise. Single source of
    /// truth for every consumer of the config value — channel construction
    /// and the readiness probe must agree on the directory.
    pub fn resolve_state_dir(configured: Option<&str>) -> PathBuf {
        match configured {
            Some(path) => PathBuf::from(shellexpand::tilde(path).as_ref()),
            None => Self::default_state_dir(),
        }
    }

    /// Read `account.json` from a state directory, if present and parseable.
    fn read_account_data(state_dir: &Path) -> Option<AccountData> {
        let data = std::fs::read_to_string(state_dir.join(ACCOUNT_FILE)).ok()?;
        serde_json::from_str::<AccountData>(&data).ok()
    }

    /// Channel-owned persisted-login probe: reports whether this state
    /// directory holds the same signal [`Self::load_persisted_state`] uses
    /// to resume a session without a fresh QR scan — an `account.json`
    /// carrying a non-empty bot token. Read-only; never creates files.
    pub fn has_persisted_login(state_dir: &Path) -> bool {
        Self::read_account_data(state_dir)
            .and_then(|account| account.token)
            .is_some_and(|token| !token.is_empty())
    }

    /// Channel-owned relink hook: delete the persisted login state so the
    /// next channel start finds no session and begins a fresh QR pairing.
    ///
    /// Removes exactly the files this module persists — [`ACCOUNT_FILE`]
    /// (the bot token, i.e. the credential) and [`SYNC_FILE`] (the sync
    /// cursor, which belongs to the replaced session) — and never the
    /// directory itself. Returns the paths actually removed; an already
    /// absent file is not an error, so relinking an unpaired channel is a
    /// safe no-op that returns an empty list.
    ///
    /// This only clears disk state. A currently running channel keeps its
    /// in-memory token until it is restarted; callers own scheduling that
    /// restart (e.g. a daemon reload).
    pub fn clear_persisted_login(state_dir: &Path) -> std::io::Result<Vec<String>> {
        let mut removed = Vec::new();
        for file in [ACCOUNT_FILE, SYNC_FILE] {
            let path = state_dir.join(file);
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(path.display().to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    }

    /// Load persisted token and cursor from state_dir.
    fn load_persisted_state(&mut self) {
        if let Some(account) = Self::read_account_data(&self.state_dir) {
            if let Some(ref token) = account.token
                && !token.is_empty()
            {
                *self.bot_token.write().unwrap() = Some(token.clone());
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "loaded persisted bot token"
                );
            }
            if let Some(ref id) = account.account_id {
                *self.account_id.write().unwrap() = Some(id.clone());
            }
        }

        let sync_path = self.state_dir.join(SYNC_FILE);
        if let Ok(data) = std::fs::read_to_string(&sync_path)
            && let Ok(sync) = serde_json::from_str::<SyncData>(&data)
        {
            if !sync.get_updates_buf.is_empty() {
                *self.cursor.lock() = sync.get_updates_buf;
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "loaded persisted sync cursor"
                );
            }
            if !sync.context_tokens.is_empty() {
                *self.context_tokens.lock() = sync.context_tokens;
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "loaded persisted context tokens"
                );
            }
        }
    }

    /// Save account data to disk.
    ///
    /// Async so the QR-login path does not block the runtime on filesystem
    /// I/O. The write itself runs on `spawn_blocking` and uses
    /// [`write_private`] (atomic replace + best-effort fsync). Errors
    /// propagate to the caller.
    async fn save_account_data(
        &self,
        token: &str,
        account_id: &str,
        user_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let state_dir = self.state_dir.clone();
        let data = AccountData {
            token: Some(token.to_string()),
            account_id: Some(account_id.to_string()),
            base_url: Some(self.api_base_url.clone()),
            user_id: user_id.map(String::from),
            saved_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        let json =
            serde_json::to_string_pretty(&data).context("failed to serialize account data")?;
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&state_dir).context("failed to create state dir")?;
            write_private(&state_dir.join(ACCOUNT_FILE), json.as_bytes())
                .context("failed to write account data")
        })
        .await
        .context("save_account_data task join")?
    }

    /// Save sync cursor and context tokens to disk (blocking).
    ///
    /// Production async callers use [`Self::save_sync_data_async`]. Tests call
    /// this directly against the same persist helper.
    #[cfg(test)]
    fn save_sync_data(&self) -> anyhow::Result<()> {
        persist_sync_data(
            &self.state_dir,
            self.cursor.lock().clone(),
            self.context_tokens.lock().clone(),
        )
    }

    /// Non-blocking wrapper around [`Self::save_sync_data`].
    async fn save_sync_data_async(&self) -> anyhow::Result<()> {
        let state_dir = self.state_dir.clone();
        let cursor = self.cursor.lock().clone();
        let context_tokens = self.context_tokens.lock().clone();
        tokio::task::spawn_blocking(move || persist_sync_data(&state_dir, cursor, context_tokens))
            .await
            .context("save_sync_data task join")?
    }

    async fn set_context_token(&self, user_id: &str, token: &str) -> anyhow::Result<()> {
        self.context_tokens
            .lock()
            .insert(user_id.to_string(), token.to_string());
        self.save_sync_data_async().await
    }

    fn has_token(&self) -> bool {
        self.bot_token.read().map(|t| t.is_some()).unwrap_or(false)
    }

    fn get_token(&self) -> Option<String> {
        self.bot_token.read().ok().and_then(|t| t.clone())
    }

    fn get_context_token(&self, user_id: &str) -> Option<String> {
        self.context_tokens.lock().get(user_id).cloned()
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        let peers = (self.peer_resolver)();
        crate::allowlist::is_user_allowed(&peers, user_id, crate::allowlist::Match::Sensitive)
    }

    async fn persist_allowed_identity(&self, identity: &str) -> anyhow::Result<()> {
        crate::identity_persist::persist_external_peer(
            self.persist.as_ref(),
            "wechat",
            &self.alias,
            identity,
        )
        .await
    }

    fn extract_bind_code(text: &str) -> Option<&str> {
        let mut parts = text.split_whitespace();
        let command = parts.next()?;
        if command != WECHAT_BIND_COMMAND {
            return None;
        }
        parts.next().map(str::trim).filter(|code| !code.is_empty())
    }

    fn api_url(&self, endpoint: &str) -> String {
        let base = self.api_base_url.trim_end_matches('/');
        format!("{base}/ilink/bot/{endpoint}")
    }

    fn cdn_download_url(&self, encrypted_query_param: &str) -> String {
        let base = self.cdn_base_url.trim_end_matches('/');
        format!(
            "{base}/download?encrypted_query_param={}",
            urlencoding::encode(encrypted_query_param)
        )
    }

    fn cdn_upload_url(&self, upload_param: &str, filekey: &str) -> String {
        let base = self.cdn_base_url.trim_end_matches('/');
        format!(
            "{base}/upload?encrypted_query_param={}&filekey={}",
            urlencoding::encode(upload_param),
            urlencoding::encode(filekey)
        )
    }

    fn canonicalize_within_workspace(
        candidate: &Path,
        workspace_dir: &Path,
        raw_target: &str,
    ) -> anyhow::Result<PathBuf> {
        let Ok(candidate_canon) = std::fs::canonicalize(candidate) else {
            return Ok(candidate.to_path_buf());
        };
        let workspace_canon = std::fs::canonicalize(workspace_dir).with_context(|| {
            format!(
                "workspace_dir {} could not be canonicalized",
                workspace_dir.display()
            )
        })?;
        if !candidate_canon.starts_with(&workspace_canon) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "attachment path {} canonicalizes to {} which escapes workspace {}",
                    raw_target,
                    candidate_canon.display(),
                    workspace_canon.display(),
                )
            );
            anyhow::bail!(
                "attachment path {} canonicalizes to {} which escapes workspace {}",
                raw_target,
                candidate_canon.display(),
                workspace_canon.display(),
            );
        }
        Ok(candidate_canon)
    }

    fn resolve_local_attachment_path(&self, target: &str) -> anyhow::Result<PathBuf> {
        let workspace_dir = self.workspace_dir.as_deref().ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "workspace directory is not configured; cannot resolve local attachment path"
            );
            anyhow::Error::msg(
                "workspace directory is not configured; cannot resolve local attachment path",
            )
        })?;

        let target = target.trim();
        let target = target.strip_prefix("file://").unwrap_or(target);

        let workspace_normalized = normalize_lexical(workspace_dir);

        // `/workspace/...` is interpreted as relative to the workspace root.
        if let Some(rel) = target.strip_prefix("/workspace/") {
            let resolved = resolve_under(workspace_dir, rel).with_context(|| {
                format!(
                    "attachment path {} escapes workspace {}",
                    target,
                    workspace_dir.display()
                )
            })?;
            return Self::canonicalize_within_workspace(&resolved, workspace_dir, target);
        }

        // Absolute paths are allowed only if they are already inside the workspace.
        let candidate = Path::new(target);
        if candidate.is_absolute() {
            let normalized = normalize_lexical(candidate);
            if !normalized.starts_with(&workspace_normalized) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!(
                        "attachment path {} escapes workspace {}, rejected",
                        target,
                        workspace_dir.display()
                    )
                );
                anyhow::bail!(
                    "attachment path {} escapes workspace {}",
                    target,
                    workspace_dir.display()
                );
            }
            return Self::canonicalize_within_workspace(&normalized, workspace_dir, target);
        }

        // Relative paths are resolved under the workspace root.
        let resolved = resolve_under(workspace_dir, target).with_context(|| {
            format!(
                "attachment path {} escapes workspace {}",
                target,
                workspace_dir.display()
            )
        })?;
        Self::canonicalize_within_workspace(&resolved, workspace_dir, target)
    }

    fn remote_file_name(
        &self,
        url: &str,
        content_type: Option<&str>,
        kind: WeChatAttachmentKind,
    ) -> String {
        let cleaned_url = url
            .split('?')
            .next()
            .unwrap_or(url)
            .split('#')
            .next()
            .unwrap_or(url);

        if let Some(last_segment) = cleaned_url.rsplit('/').next()
            && let Some(name) = sanitize_attachment_filename(last_segment)
            && Path::new(&name).extension().is_some()
        {
            return name;
        }

        let ext = content_type
            .and_then(|value| value.split(';').next())
            .and_then(mime_guess::get_mime_extensions_str)
            .and_then(|exts: &[&str]| exts.first().copied())
            .unwrap_or(kind.default_extension());

        format!(
            "wechat_attachment_{}.{}",
            uuid::Uuid::new_v4().simple(),
            ext
        )
    }

    async fn download_remote_attachment(
        &self,
        url: &str,
        kind: WeChatAttachmentKind,
    ) -> anyhow::Result<WeChatMediaPayload> {
        if !url.starts_with("https://") {
            anyhow::bail!("refusing non-HTTPS attachment URL: {url}");
        }
        let resp = self
            .client
            .get(url)
            .timeout(API_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("attachment download failed: {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("attachment download failed ({status}): {body}");
        }

        if let Some(len) = resp.content_length()
            && len > WECHAT_MEDIA_MAX_BYTES
        {
            anyhow::bail!(
                "attachment Content-Length ({len} bytes) exceeds {} MB limit",
                WECHAT_MEDIA_MAX_BYTES / (1024 * 1024)
            );
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = resp.bytes().await?.to_vec();

        if bytes.len() as u64 > WECHAT_MEDIA_MAX_BYTES {
            anyhow::bail!(
                "attachment exceeds {} MB limit",
                WECHAT_MEDIA_MAX_BYTES / (1024 * 1024)
            );
        }

        Ok(WeChatMediaPayload {
            file_name: self.remote_file_name(url, content_type.as_deref(), kind),
            bytes,
        })
    }

    async fn load_attachment_payload(
        &self,
        attachment: &WeChatAttachment,
    ) -> anyhow::Result<WeChatMediaPayload> {
        let target = attachment.target.trim();
        if is_remote_url(target) {
            return self
                .download_remote_attachment(target, attachment.kind)
                .await;
        }

        let path = self.resolve_local_attachment_path(target)?;
        if !path.exists() {
            anyhow::bail!("attachment path not found: {}", path.display());
        }

        let file_name = sanitize_attachment_filename(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment.bin"),
        )
        .unwrap_or_else(|| {
            format!(
                "wechat_attachment_{}.{}",
                uuid::Uuid::new_v4().simple(),
                attachment.kind.default_extension()
            )
        });

        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("attachment read failed: {}", path.display()))?;
        if bytes.len() as u64 > WECHAT_MEDIA_MAX_BYTES {
            anyhow::bail!(
                "attachment exceeds {} MB limit",
                WECHAT_MEDIA_MAX_BYTES / (1024 * 1024)
            );
        }

        Ok(WeChatMediaPayload { bytes, file_name })
    }

    async fn request_upload_param(
        &self,
        to: &str,
        kind: WeChatAttachmentKind,
        payload: &WeChatMediaPayload,
        aes_key: &[u8; 16],
        filekey: &str,
    ) -> anyhow::Result<String> {
        let token = self
            .get_token()
            .context("not logged in, cannot upload attachment")?;
        let body = serde_json::json!({
            "filekey": filekey,
            "media_type": kind.upload_media_type(),
            "to_user_id": to,
            "rawsize": payload.bytes.len(),
            "rawfilemd5": format!("{:x}", md5::compute(&payload.bytes)),
            "filesize": aes_ecb_padded_size(payload.bytes.len()),
            "no_need_thumb": true,
            "aeskey": hex::encode(aes_key),
            "base_info": build_base_info()
        });

        let resp = self
            .client
            .post(self.api_url("getuploadurl"))
            .headers(build_headers(Some(&token)))
            .json(&body)
            .timeout(API_TIMEOUT)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("getUploadUrl failed ({status}): {body}");
        }

        let data: serde_json::Value = resp.json().await?;
        data.get("upload_param")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .context("getUploadUrl returned no upload_param")
    }

    async fn upload_to_cdn(
        &self,
        upload_param: &str,
        filekey: &str,
        ciphertext: &[u8],
    ) -> anyhow::Result<String> {
        let url = self.cdn_upload_url(upload_param, filekey);
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=3 {
            let resp = self
                .client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(ciphertext.to_vec())
                .timeout(API_TIMEOUT)
                .send()
                .await;

            match resp {
                Ok(resp) if resp.status().is_success() => {
                    let encrypted_param = resp
                        .headers()
                        .get("x-encrypted-param")
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .context("CDN upload missing x-encrypted-param header")?;
                    return Ok(encrypted_param);
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "attempt": attempt,
                                "status": status.as_u16(),
                                "body": body,
                                "phase": "cdn_upload",
                            })),
                        "wechat: CDN upload failed (non-success status)"
                    );
                    let error = anyhow::Error::msg(format!(
                        "CDN upload failed on attempt {attempt} ({status}): {body}"
                    ));
                    if status.is_client_error() {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
                Err(err) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "attempt": attempt,
                                "phase": "cdn_upload",
                                "error": format!("{}", err),
                            })),
                        "wechat: CDN upload request failed"
                    );
                    last_error = Some(anyhow::Error::msg(format!(
                        "CDN upload request failed on attempt {attempt}: {err}"
                    )));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"phase": "cdn_upload"})),
                "wechat: CDN upload exhausted retries"
            );
            anyhow::Error::msg("CDN upload failed")
        }))
    }

    async fn upload_media_payload(
        &self,
        to: &str,
        kind: WeChatAttachmentKind,
        payload: &WeChatMediaPayload,
    ) -> anyhow::Result<UploadedWeChatMedia> {
        let filekey = uuid::Uuid::new_v4().simple().to_string();
        let aes_key: [u8; 16] = rand::random();
        let upload_param = self
            .request_upload_param(to, kind, payload, &aes_key, &filekey)
            .await?;
        let ciphertext = encrypt_aes_ecb(&payload.bytes, &aes_key)?;
        let encrypted_query_param = self
            .upload_to_cdn(&upload_param, &filekey, &ciphertext)
            .await?;

        // CDNMedia `aes_key` must be base64(hex(key)).
        // WeChat client base64-decodes then hex-decodes to recover the 16 bytes.
        let aes_key_base64 = base64::engine::general_purpose::STANDARD.encode(hex::encode(aes_key));

        Ok(UploadedWeChatMedia {
            encrypted_query_param,
            aes_key_base64,
            raw_size: payload.bytes.len(),
            encrypted_size: ciphertext.len(),
        })
    }

    fn find_inbound_attachment(
        items: &[serde_json::Value],
        message_id: &str,
    ) -> Option<InboundAttachmentSpec> {
        fn default_name(kind: WeChatAttachmentKind, message_id: &str) -> String {
            let safe_id: String = message_id
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                .collect();
            match kind {
                WeChatAttachmentKind::Image => format!("wechat_{safe_id}.jpg"),
                WeChatAttachmentKind::Document => format!("wechat_{safe_id}.bin"),
                WeChatAttachmentKind::Video => format!("wechat_{safe_id}.mp4"),
                WeChatAttachmentKind::Audio => format!("wechat_{safe_id}.mp3"),
                WeChatAttachmentKind::Voice => format!("wechat_{safe_id}.silk"),
            }
        }

        fn parse_item(item: &serde_json::Value, message_id: &str) -> Option<InboundAttachmentSpec> {
            let item_type = item
                .get("type")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())?;
            match item_type {
                ITEM_TYPE_IMAGE => {
                    let image_item = item.get("image_item")?;
                    let media = image_item.get("media")?;
                    let encrypted_query_param =
                        media.get("encrypt_query_param")?.as_str()?.to_string();
                    let aes_key = image_item
                        .get("aeskey")
                        .and_then(|value| value.as_str())
                        .or_else(|| media.get("aes_key").and_then(|value| value.as_str()))
                        .map(str::to_string);
                    Some(InboundAttachmentSpec {
                        kind: WeChatAttachmentKind::Image,
                        encrypted_query_param,
                        aes_key,
                        file_name: default_name(WeChatAttachmentKind::Image, message_id),
                    })
                }
                ITEM_TYPE_FILE => {
                    let file_item = item.get("file_item")?;
                    let media = file_item.get("media")?;
                    let encrypted_query_param =
                        media.get("encrypt_query_param")?.as_str()?.to_string();
                    let aes_key = media
                        .get("aes_key")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    let file_name = file_item
                        .get("file_name")
                        .and_then(|value| value.as_str())
                        .and_then(sanitize_attachment_filename)
                        .unwrap_or_else(|| {
                            default_name(WeChatAttachmentKind::Document, message_id)
                        });
                    Some(InboundAttachmentSpec {
                        kind: WeChatAttachmentKind::Document,
                        encrypted_query_param,
                        aes_key,
                        file_name,
                    })
                }
                ITEM_TYPE_VIDEO => {
                    let video_item = item.get("video_item")?;
                    let media = video_item.get("media")?;
                    let encrypted_query_param =
                        media.get("encrypt_query_param")?.as_str()?.to_string();
                    let aes_key = media
                        .get("aes_key")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    Some(InboundAttachmentSpec {
                        kind: WeChatAttachmentKind::Video,
                        encrypted_query_param,
                        aes_key,
                        file_name: default_name(WeChatAttachmentKind::Video, message_id),
                    })
                }
                ITEM_TYPE_VOICE => {
                    let voice_item = item.get("voice_item")?;
                    let media = voice_item.get("media")?;
                    let encrypted_query_param =
                        media.get("encrypt_query_param")?.as_str()?.to_string();
                    let aes_key = media
                        .get("aes_key")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    Some(InboundAttachmentSpec {
                        kind: WeChatAttachmentKind::Voice,
                        encrypted_query_param,
                        aes_key,
                        file_name: default_name(WeChatAttachmentKind::Voice, message_id),
                    })
                }
                _ => None,
            }
        }

        for item in items {
            if let Some(spec) = parse_item(item, message_id) {
                return Some(spec);
            }
        }

        for item in items {
            let Some(ref_item) = item
                .get("ref_msg")
                .and_then(|value| value.get("message_item"))
            else {
                continue;
            };

            if let Some(spec) = parse_item(ref_item, message_id) {
                return Some(spec);
            }
        }

        None
    }

    async fn download_inbound_attachment(
        &self,
        spec: &InboundAttachmentSpec,
    ) -> Result<Vec<u8>, AttachmentBuildFailure> {
        let resp = self
            .client
            .get(self.cdn_download_url(&spec.encrypted_query_param))
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                AttachmentBuildFailure::transient(format!("attachment request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttachmentBuildFailure::classify_status(status, &body));
        }

        if let Some(len) = resp.content_length()
            && len > WECHAT_MEDIA_MAX_BYTES
        {
            return Err(AttachmentBuildFailure::permanent(format!(
                "inbound attachment Content-Length ({len} bytes) exceeds {} MB limit",
                WECHAT_MEDIA_MAX_BYTES / (1024 * 1024)
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| {
                AttachmentBuildFailure::transient(format!("attachment body read failed: {e}"))
            })?
            .to_vec();
        if bytes.len() as u64 > WECHAT_MEDIA_MAX_BYTES {
            return Err(AttachmentBuildFailure::permanent(format!(
                "inbound attachment exceeds {} MB limit",
                WECHAT_MEDIA_MAX_BYTES / (1024 * 1024)
            )));
        }

        match spec.aes_key.as_deref() {
            Some(aes_key) if !aes_key.is_empty() => {
                let key = parse_aes_key(aes_key)
                    .map_err(|e| AttachmentBuildFailure::permanent(e.to_string()))?;
                decrypt_aes_ecb(&bytes, &key)
                    .map_err(|e| AttachmentBuildFailure::transient(e.to_string()))
            }
            _ => Ok(bytes),
        }
    }

    async fn try_build_attachment_content(
        &self,
        items: &[serde_json::Value],
        message_id: &str,
    ) -> AttachmentDisposition {
        let Some(workspace_dir) = self.workspace_dir.as_ref() else {
            return AttachmentDisposition::None;
        };
        let Some(spec) = Self::find_inbound_attachment(items, message_id) else {
            return AttachmentDisposition::None;
        };
        let bytes = match self.download_inbound_attachment(&spec).await {
            Ok(bytes) => bytes,
            Err(failure) => return AttachmentDisposition::from_failure(failure, spec.kind),
        };

        let save_dir = workspace_dir.join("wechat_files");
        if let Err(err) = tokio::fs::create_dir_all(&save_dir).await {
            return AttachmentDisposition::from_failure(
                AttachmentBuildFailure::transient(format!(
                    "Failed to create WeChat attachment dir: {err}"
                )),
                spec.kind,
            );
        }

        let local_path = save_dir.join(&spec.file_name);
        if let Err(err) = tokio::fs::write(&local_path, bytes).await {
            return AttachmentDisposition::from_failure(
                AttachmentBuildFailure::transient(format!(
                    "Failed to save WeChat attachment to {}: {err}",
                    local_path.display()
                )),
                spec.kind,
            );
        }

        AttachmentDisposition::Ready(format_attachment_content(
            spec.kind,
            &spec.file_name,
            &local_path,
        ))
    }

    /// Stage every authorized message in a getUpdates batch before any
    /// `tx.send`. A transient attachment failure discards the staged items
    /// and returns `StopBatch` so the caller leaves the cursor uncommitted.
    async fn stage_inbound_batch(&self, msgs: &[serde_json::Value]) -> BatchOutcome {
        let mut staged = Vec::new();

        for msg in msgs {
            let from_user_id = msg
                .get("from_user_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if from_user_id.is_empty() {
                continue;
            }

            if let Some(ctx_token) = msg.get("context_token").and_then(|v| v.as_str())
                && !ctx_token.is_empty()
                && let Err(e) = self.set_context_token(from_user_id, ctx_token).await
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "failed to persist WeChat context token"
                );
            }

            let items = msg
                .get("item_list")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let message_id = msg
                .get("message_id")
                .and_then(|v| v.as_u64())
                .map(|id| id.to_string())
                .unwrap_or_else(|| format!("wechat_{}", uuid::Uuid::new_v4()));

            let text = extract_text_from_items(&items);

            if !self.is_user_allowed(from_user_id) {
                staged.push(StagedInbound::Unauthorized {
                    from_user_id: from_user_id.to_string(),
                    text,
                });
                continue;
            }

            let attachment = self.try_build_attachment_content(&items, &message_id).await;
            let content = match (attachment, text.is_empty()) {
                (AttachmentDisposition::RetryTransient, _) => {
                    return BatchOutcome::StopBatch;
                }
                (AttachmentDisposition::Ready(marker), true) => marker,
                (AttachmentDisposition::Ready(marker), false) => {
                    format!("{marker}\n\n{text}")
                }
                (AttachmentDisposition::SkipPermanent(notice), false) => {
                    format!("{notice}\n\n{text}")
                }
                (AttachmentDisposition::None, false) => text,
                (AttachmentDisposition::None | AttachmentDisposition::SkipPermanent(_), true) => {
                    continue;
                }
            };

            let timestamp = msg
                .get("create_time_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                / 1000;

            staged.push(StagedInbound::Deliver(Box::new(ChannelMessage {
                id: message_id,
                sender: from_user_id.to_string(),
                reply_target: from_user_id.to_string(),
                content,
                channel: "wechat".to_string(),
                channel_alias: Some(self.alias.clone()),
                timestamp,
                thread_ts: None,
                interruption_scope_id: None,
                attachments: Vec::new(),
                subject: None,
                ..Default::default()
            })));
        }

        BatchOutcome::Ready(staged)
    }

    /// Perform QR-code login flow. Returns (bot_token, account_id, user_id).
    async fn qr_login(&self) -> anyhow::Result<(String, String, Option<String>)> {
        let mut qr_refresh_count = 0u32;

        loop {
            qr_refresh_count += 1;
            if qr_refresh_count > MAX_QR_REFRESH {
                let max = MAX_QR_REFRESH.to_string();
                let reason = wechat_cli_string_with_args(
                    "cli-wechat-qr-expired-giving-up",
                    &[("max", &max)],
                );
                crate::login_events::LoginEvent::Failed { reason: &reason }.emit(
                    self.name(),
                    &self.alias,
                    "WeChat QR login gave up after repeated expiry",
                );
                anyhow::bail!("{reason}");
            }

            // Fetch QR code
            let qr_url = format!("{}?bot_type=3", self.api_url("get_bot_qrcode"));
            let resp = self
                .client
                .get(&qr_url)
                .timeout(API_TIMEOUT)
                .send()
                .await
                .with_context(|| wechat_cli_string("cli-wechat-qr-fetch-failed"))?;

            if !resp.status().is_success() {
                let status = resp.status().to_string();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "{}",
                    wechat_cli_string_with_args(
                        "cli-wechat-qr-fetch-status-failed",
                        &[("status", &status), ("body", &body)],
                    )
                );
            }

            let qr_data: serde_json::Value = resp.json().await?;
            let qrcode = qr_data
                .get("qrcode")
                .and_then(|v| v.as_str())
                .with_context(|| {
                    wechat_cli_string_with_args(
                        "cli-wechat-missing-response-field",
                        &[("field", "qrcode")],
                    )
                })?
                .to_string();
            let qrcode_img_url = qr_data
                .get("qrcode_img_content")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Display QR code
            let qr_attempt = qr_refresh_count.to_string();
            let qr_max = MAX_QR_REFRESH.to_string();
            println!(
                "\n  {}",
                wechat_cli_string_with_args(
                    "cli-wechat-qr-login",
                    &[("attempt", &qr_attempt), ("max", &qr_max)],
                )
            );
            println!("  {}\n", wechat_cli_string("cli-wechat-scan-to-connect"));
            let qr_payload = if qrcode_img_url.is_empty() {
                qrcode.as_str()
            } else {
                qrcode_img_url
            };
            crate::login_events::LoginEvent::Qr {
                payload: qr_payload,
                image_url: (!qrcode_img_url.is_empty()).then_some(qrcode_img_url),
                attempt: Some(qr_refresh_count),
                max_attempts: Some(MAX_QR_REFRESH),
            }
            .emit(
                self.name(),
                &self.alias,
                "WeChat login QR code ready (scan with the WeChat app)",
            );
            match render_login_qr(qr_payload) {
                Ok(qr) => println!("{qr}"),
                Err(err) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", err)})),
                        "failed to render terminal QR code"
                    )
                }
            }
            if !qrcode_img_url.is_empty() {
                println!(
                    "  {}",
                    wechat_cli_string_with_args("cli-wechat-qr-url", &[("url", qrcode_img_url)],)
                );
            }

            // Poll for scan status
            let deadline = std::time::Instant::now() + QR_SCAN_TIMEOUT;
            let mut scanned_printed = false;

            while std::time::Instant::now() < deadline {
                let status_url = format!(
                    "{}?qrcode={}",
                    self.api_url("get_qrcode_status"),
                    urlencoding::encode(&qrcode)
                );
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert("iLink-App-ClientVersion", "1".parse().unwrap());

                let poll_result = tokio::time::timeout(
                    QR_POLL_TIMEOUT + Duration::from_secs(5),
                    self.client
                        .get(&status_url)
                        .headers(headers)
                        .timeout(QR_POLL_TIMEOUT)
                        .send(),
                )
                .await;

                let resp = match poll_result {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "QR poll error"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    Err(_) => {
                        // Client-side timeout, normal for long-poll
                        continue;
                    }
                };

                let status: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "QR poll parse error"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                let status_str = status
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("wait");

                match status_str {
                    "wait" => {}
                    "scaned" => {
                        if !scanned_printed {
                            println!("  {}", wechat_cli_string("cli-wechat-scanned-confirm"));
                            crate::login_events::LoginEvent::Scanned.emit(
                                self.name(),
                                &self.alias,
                                "WeChat QR code scanned — waiting for in-app confirmation",
                            );
                            scanned_printed = true;
                        }
                    }
                    "expired" => {
                        println!(
                            "  {}",
                            wechat_cli_string("cli-wechat-qr-expired-refreshing")
                        );
                        crate::login_events::LoginEvent::Expired {
                            attempt: qr_refresh_count,
                            max_attempts: MAX_QR_REFRESH,
                        }
                        .emit(
                            self.name(),
                            &self.alias,
                            "WeChat login QR code expired",
                        );
                        break; // Will loop back and get a new QR code
                    }
                    "confirmed" => {
                        let bot_token = status
                            .get("bot_token")
                            .and_then(|v| v.as_str())
                            .with_context(|| {
                                wechat_cli_string_with_args(
                                    "cli-wechat-login-confirmed-missing-field",
                                    &[("field", "bot_token")],
                                )
                            })?
                            .to_string();
                        let account_id = status
                            .get("ilink_bot_id")
                            .and_then(|v| v.as_str())
                            .with_context(|| {
                                wechat_cli_string_with_args(
                                    "cli-wechat-login-confirmed-missing-field",
                                    &[("field", "ilink_bot_id")],
                                )
                            })?
                            .to_string();
                        let user_id = status
                            .get("ilink_user_id")
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        println!("  {}", wechat_cli_string("cli-wechat-connected"));
                        crate::login_events::LoginEvent::Connected.emit(
                            self.name(),
                            &self.alias,
                            "WeChat login confirmed — channel connected",
                        );
                        return Ok((bot_token, account_id, user_id));
                    }
                    other => {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"other": other})),
                            "QR status"
                        );
                    }
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            // If we reach here without returning, the QR expired or timed out.
            // Loop will try again up to MAX_QR_REFRESH times.
        }
    }

    /// Ensure we have a valid bot token, performing QR login if needed.
    async fn ensure_logged_in(&self) -> anyhow::Result<()> {
        if self.has_token() {
            return Ok(());
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "no persisted token, starting QR login..."
        );
        let (token, account_id, user_id) = self.qr_login().await?;

        // Save to memory
        if let Ok(mut t) = self.bot_token.write() {
            *t = Some(token.clone());
        }
        if let Ok(mut a) = self.account_id.write() {
            *a = Some(account_id.clone());
        }

        // If a user scanned, persist them as an allowed peer
        if let Some(ref uid) = user_id
            && let Err(e) = self.persist_allowed_identity(uid).await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e), "uid": uid})),
                "failed to persist scanned identity"
            );
        }

        // Persist to disk
        self.save_account_data(&token, &account_id, user_id.as_deref())
            .await?;

        Ok(())
    }

    async fn send_message_items(
        &self,
        to: &str,
        item_list: Vec<serde_json::Value>,
        context_token: Option<&str>,
    ) -> anyhow::Result<()> {
        let token = self.get_token().context("not logged in, cannot send")?;

        let client_id = format!("zeroclaw-{}", uuid::Uuid::new_v4());
        let body = serde_json::json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to,
                "client_id": client_id,
                "message_type": MESSAGE_TYPE_BOT,
                "message_state": MESSAGE_STATE_FINISH,
                "item_list": item_list,
                "context_token": context_token.unwrap_or("")
            },
            "base_info": build_base_info()
        });

        let resp = self
            .client
            .post(self.api_url("sendmessage"))
            .headers(build_headers(Some(&token)))
            .json(&body)
            .timeout(API_TIMEOUT)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessage failed ({status}): {err}");
        }

        // The API reports failures as HTTP 200 with a non-zero ret/errcode
        // in the body; a status check alone silently drops the message.
        let body = resp
            .text()
            .await
            .context("failed to read sendMessage response body")?;
        if let Some(err) = sendmessage_body_error(&body) {
            anyhow::bail!("sendMessage failed ({err})");
        }

        Ok(())
    }

    /// Send a text message via iLink API.
    async fn send_text(
        &self,
        to: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_message_items(
            to,
            vec![serde_json::json!({
                "type": ITEM_TYPE_TEXT,
                "text_item": { "text": markdown_to_plain_text(text) }
            })],
            context_token,
        )
        .await
    }

    async fn send_attachment(
        &self,
        to: &str,
        attachment: &WeChatAttachment,
        context_token: Option<&str>,
    ) -> anyhow::Result<()> {
        let payload = self.load_attachment_payload(attachment).await?;
        let uploaded = self
            .upload_media_payload(to, attachment.kind, &payload)
            .await?;

        let item = match attachment.kind {
            WeChatAttachmentKind::Image => serde_json::json!({
                "type": ITEM_TYPE_IMAGE,
                "image_item": {
                    "media": {
                        "encrypt_query_param": uploaded.encrypted_query_param,
                        "aes_key": uploaded.aes_key_base64,
                        "encrypt_type": 1
                    },
                    "mid_size": uploaded.encrypted_size
                }
            }),
            WeChatAttachmentKind::Video => serde_json::json!({
                "type": ITEM_TYPE_VIDEO,
                "video_item": {
                    "media": {
                        "encrypt_query_param": uploaded.encrypted_query_param,
                        "aes_key": uploaded.aes_key_base64,
                        "encrypt_type": 1
                    },
                    "video_size": uploaded.encrypted_size
                }
            }),
            WeChatAttachmentKind::Document
            | WeChatAttachmentKind::Audio
            | WeChatAttachmentKind::Voice => serde_json::json!({
                "type": ITEM_TYPE_FILE,
                "file_item": {
                    "media": {
                        "encrypt_query_param": uploaded.encrypted_query_param,
                        "aes_key": uploaded.aes_key_base64,
                        "encrypt_type": 1
                    },
                    "file_name": payload.file_name,
                    "len": uploaded.raw_size.to_string()
                }
            }),
        };

        self.send_message_items(to, vec![item], context_token).await
    }

    /// Fetch typing_ticket for a user via getconfig.
    async fn fetch_typing_ticket(&self, user_id: &str) -> Option<String> {
        let token = self.get_token()?;
        let context_token = self.get_context_token(user_id);

        let body = serde_json::json!({
            "ilink_user_id": user_id,
            "context_token": context_token.unwrap_or_default(),
            "base_info": build_base_info()
        });

        let resp = self
            .client
            .post(self.api_url("getconfig"))
            .headers(build_headers(Some(&token)))
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?;

        let data: serde_json::Value = resp.json().await.ok()?;
        data.get("typing_ticket")
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    /// Get or fetch typing_ticket for a user.
    async fn get_typing_ticket(&self, user_id: &str) -> Option<String> {
        // Check cache first
        if let Some(ticket) = self.typing_tickets.lock().get(user_id).cloned() {
            return Some(ticket);
        }

        // Fetch and cache
        let ticket = self.fetch_typing_ticket(user_id).await?;
        self.typing_tickets
            .lock()
            .insert(user_id.to_string(), ticket.clone());
        Some(ticket)
    }

    /// Handle an unauthorized message (check for /bind command).
    async fn handle_unauthorized_message(&self, from_user_id: &str, text: &str) {
        if let Some(code) = Self::extract_bind_code(text) {
            if let Some(pairing) = self.pairing.as_ref() {
                match pairing.try_pair(code, from_user_id).await {
                    Ok(Some(_token)) => {
                        if let Err(e) = self.persist_allowed_identity(from_user_id).await {
                            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"from_user_id": from_user_id, "e": e.to_string()})), "failed to persist bound identity");
                        }
                        let ctx = self.get_context_token(from_user_id);
                        let reply = wechat_cli_string("cli-wechat-bound-success");
                        let _ = self.send_text(from_user_id, &reply, ctx.as_deref()).await;
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"from_user_id": from_user_id})),
                            "user bound via pairing code"
                        );
                    }
                    Ok(None) => {
                        let ctx = self.get_context_token(from_user_id);
                        let reply = wechat_cli_string("cli-wechat-invalid-bind-code");
                        let _ = self.send_text(from_user_id, &reply, ctx.as_deref()).await;
                    }
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "pairing error"
                        );
                    }
                }
            }
        } else {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"from_user_id": from_user_id})),
                "ignoring unauthorized message from"
            );
        }
    }
}

impl ::zeroclaw_api::attribution::Attributable for WeChatChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(::zeroclaw_api::attribution::ChannelKind::Wechat)
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl Channel for WeChatChannel {
    fn name(&self) -> &str {
        "wechat"
    }

    fn supports_draft_updates(&self) -> bool {
        true
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let recipient = &message.recipient;
        let content = crate::util::strip_tool_call_tags(&message.content);
        let context_token = self.get_context_token(recipient);

        if context_token.is_none() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"recipient": recipient})),
                "no context_token for , message may fail to associate"
            );
        }

        let (text_without_markers, attachments) = parse_attachment_markers(&content);
        if !attachments.is_empty() {
            if !text_without_markers.is_empty() {
                self.send_text(recipient, &text_without_markers, context_token.as_deref())
                    .await?;
            }

            for attachment in &attachments {
                self.send_attachment(recipient, attachment, context_token.as_deref())
                    .await?;
            }
            return Ok(());
        }

        if let Some(attachment) = parse_path_only_attachment(&content) {
            return self
                .send_attachment(recipient, &attachment, context_token.as_deref())
                .await;
        }

        self.send_text(recipient, &content, context_token.as_deref())
            .await
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        // Ensure we're logged in (QR scan if needed)
        self.ensure_logged_in().await?;

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "channel listening for messages..."
        );

        let mut cursor = self.cursor.lock().clone();
        let mut long_poll_timeout_ms = LONG_POLL_TIMEOUT_MS;
        let mut consecutive_failures: u32 = 0;

        loop {
            let token = match self.get_token() {
                Some(t) => t,
                None => {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "token lost, attempting re-login..."
                    );
                    if let Err(e) = self.ensure_logged_in().await {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "re-login failed"
                        );
                        tokio::time::sleep(BACKOFF_DELAY).await;
                        continue;
                    }
                    match self.get_token() {
                        Some(t) => t,
                        None => {
                            tokio::time::sleep(BACKOFF_DELAY).await;
                            continue;
                        }
                    }
                }
            };

            let body = serde_json::json!({
                "get_updates_buf": cursor,
                "base_info": build_base_info()
            });

            let result = tokio::time::timeout(
                long_poll_client_timeout(long_poll_timeout_ms),
                self.client
                    .post(self.api_url("getupdates"))
                    .headers(build_headers(Some(&token)))
                    .json(&body)
                    .timeout(Duration::from_millis(long_poll_timeout_ms))
                    .send(),
            )
            .await;

            let resp = match result {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    consecutive_failures += 1;
                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"consecutive_failures": consecutive_failures, "MAX_CONSECUTIVE_FAILURES": MAX_CONSECUTIVE_FAILURES, "e": e.to_string()})), "getUpdates error (/)");
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        consecutive_failures = 0;
                        tokio::time::sleep(BACKOFF_DELAY).await;
                    } else {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                    continue;
                }
                Err(_) => {
                    // Client-side timeout — normal for long-poll, just retry
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        "getUpdates: client-side timeout, retrying"
                    );
                    continue;
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    consecutive_failures += 1;
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "getUpdates parse error"
                    );
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        consecutive_failures = 0;
                        tokio::time::sleep(BACKOFF_DELAY).await;
                    } else {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                    continue;
                }
            };

            // Check for API errors
            let ret = data.get("ret").and_then(|v| v.as_i64()).unwrap_or(0);
            let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(0);
            let is_error = ret != 0 || errcode != 0;

            if is_error {
                if errcode == SESSION_EXPIRED_ERRCODE || ret == SESSION_EXPIRED_ERRCODE {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        &format!(
                            "session expired (errcode {SESSION_EXPIRED_ERRCODE}), pausing for {} min",
                            SESSION_PAUSE_DURATION.as_secs() / 60
                        )
                    );
                    // Clear token so we re-login after pause
                    if let Ok(mut t) = self.bot_token.write() {
                        *t = None;
                    }
                    self.context_tokens.lock().clear();
                    if let Err(e) = self.save_sync_data_async().await {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "failed to persist WeChat sync data after session expiry"
                        );
                    }
                    tokio::time::sleep(SESSION_PAUSE_DURATION).await;
                    // Try to re-login
                    if let Err(e) = self.ensure_logged_in().await {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "re-login after session expiry failed"
                        );
                    }
                    consecutive_failures = 0;
                    continue;
                }

                consecutive_failures += 1;
                let errmsg = data.get("errmsg").and_then(|v| v.as_str()).unwrap_or("");
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"ret": ret, "errcode": errcode, "errmsg": errmsg, "consecutive_failures": consecutive_failures, "MAX_CONSECUTIVE_FAILURES": MAX_CONSECUTIVE_FAILURES})), "getUpdates failed: ret= errcode= errmsg= (/)");
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    consecutive_failures = 0;
                    tokio::time::sleep(BACKOFF_DELAY).await;
                } else {
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                continue;
            }

            consecutive_failures = 0;

            // Capture the response cursor but defer committing it (both the
            // local `cursor` and `self.cursor`/disk) until every message in
            // this batch has been successfully enqueued below. Persisting any
            // earlier would let a crash between cursor persistence and enqueue
            // completion permanently lose the batch: on restart, `listen()`
            // would reload the already-advanced cursor and never re-poll those
            // messages. `set_context_token` also persists sync data mid-batch
            // via `save_sync_data_async`; because the in-memory cursor is not
            // advanced yet, those writes serialize the old cursor.
            let next_cursor = data
                .get("get_updates_buf")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            if let Some(next_timeout) = data
                .get("longpolling_timeout_ms")
                .and_then(|v| v.as_u64())
                .filter(|timeout| *timeout > 0)
            {
                long_poll_timeout_ms = next_timeout;
            }

            // Process messages. Stage the whole batch before any `tx.send` so
            // a transient attachment failure can discard unpublished work and
            // leave the cursor uncommitted. Permanent skips (size limit,
            // unauthorized sender, CDN 400/403/404/410, invalid AES key
            // metadata) are logged and do not hold the batch.
            let msgs = data
                .get("msgs")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            match self.stage_inbound_batch(&msgs).await {
                BatchOutcome::StopBatch => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "retry_delay_secs": RETRY_DELAY.as_secs(),
                            })),
                        "Transient WeChat attachment failure; leaving cursor uncommitted so the next poll retries the batch"
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                BatchOutcome::Ready(staged) => {
                    for item in staged {
                        match item {
                            StagedInbound::Unauthorized { from_user_id, text } => {
                                self.handle_unauthorized_message(&from_user_id, &text).await;
                            }
                            StagedInbound::Deliver(channel_msg) => {
                                if tx.send(*channel_msg).await.is_err() {
                                    ::zeroclaw_log::record!(
                                        INFO,
                                        ::zeroclaw_log::Event::new(
                                            module_path!(),
                                            ::zeroclaw_log::Action::Note
                                        ),
                                        "channel receiver dropped, stopping"
                                    );
                                    // Do NOT commit `next_cursor` here: the batch is only
                                    // partially (or not at all) enqueued, so the old cursor
                                    // must stay on disk. On supervised restart `listen()`
                                    // reloads it and re-polls this batch.
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }

            // Commit the cursor only now that the whole batch has been
            // enqueued (or there was nothing to enqueue).
            //
            // At-least-once is an explicit decision: a crash in the window
            // after enqueue and before this persist, or a persist failure
            // here, can redeliver already-enqueued messages on restart. This
            // repo has no generic inbound dedup. Prefer duplicate delivery
            // over silent loss.
            //
            // A transient attachment failure never reaches this site: the
            // StopBatch arm above continues without committing, so a
            // pure-attachment message is retried instead of skipped.
            //
            // Persist failure: log and continue. The in-memory cursor has
            // advanced so this process will not re-poll the batch; the next
            // successful sync-data persist retries durability. Do not panic.
            if let Some(new_cursor) = next_cursor {
                cursor = new_cursor;
                *self.cursor.lock() = cursor.clone();
                if let Err(e) = self.save_sync_data_async().await {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "failed to persist WeChat sync cursor after enqueue"
                    );
                }
            }
        }
    }

    async fn health_check(&self) -> bool {
        let token = match self.get_token() {
            Some(t) => t,
            None => return false,
        };

        // Use getconfig with a dummy user as a health check
        let body = serde_json::json!({
            "ilink_user_id": "",
            "context_token": "",
            "base_info": build_base_info()
        });

        match tokio::time::timeout(
            Duration::from_secs(5),
            self.client
                .post(self.api_url("getconfig"))
                .headers(build_headers(Some(&token)))
                .json(&body)
                .send(),
        )
        .await
        {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        }
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.stop_typing(recipient).await?;

        let token = match self.get_token() {
            Some(t) => t,
            None => return Ok(()),
        };

        let typing_ticket = match self.get_typing_ticket(recipient).await {
            Some(t) => t,
            None => return Ok(()),
        };

        let client = self.client.clone();
        let url = self.api_url("sendtyping");
        let user_id = recipient.to_string();

        let handle = zeroclaw_spawn::spawn!(async move {
            loop {
                let body = serde_json::json!({
                    "ilink_user_id": &user_id,
                    "typing_ticket": &typing_ticket,
                    "status": 1,
                    "base_info": build_base_info()
                });
                let _ = client
                    .post(&url)
                    .headers(build_headers(Some(&token)))
                    .json(&body)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
                // Refresh typing indicator every 4 seconds
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });

        *self.typing_handle.lock() = Some(handle);
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        let mut guard = self.typing_handle.lock();
        if let Some(handle) = guard.take() {
            handle.abort();
        }
        Ok(())
    }

    async fn send_draft(&self, _msg: &SendMessage) -> anyhow::Result<Option<String>> {
        // TODO: Re-enable placeholder if WeChat adds message edit/revoke support.
        // Current behavior: Return draft_id without sending placeholder.
        // The final response will be sent in finalize_draft().
        let draft_id = format!("draft_{}", uuid::Uuid::new_v4());
        Ok(Some(draft_id))
    }

    async fn update_draft(
        &self,
        _recipient: &str,
        _draft_id: &str,
        _content: &str,
    ) -> anyhow::Result<()> {
        // WeChat iLink doesn't support message editing.
        // We accumulate deltas in the draft_updater task and only send the final
        // message in finalize_draft(). This method is a no-op.
        Ok(())
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        _draft_id: &str,
        content: &str,
        _suppress_voice: bool,
    ) -> anyhow::Result<()> {
        // Send the final accumulated response
        let result = self
            .send(&SendMessage::new(
                content.to_string(),
                recipient.to_string(),
            ))
            .await;
        let _ = self.stop_typing(recipient).await; // Always stop the typing indicator
        result
    }

    async fn cancel_draft(&self, recipient: &str, _draft_id: &str) -> anyhow::Result<()> {
        self.stop_typing(recipient).await
    }

    async fn update_draft_progress(
        &self,
        recipient: &str,
        _draft_id: &str,
        _progress: &str,
    ) -> anyhow::Result<()> {
        // Use the typing indicator instead of message updates
        let _ = self.start_typing(recipient).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
