//! WhatsApp Web channel using wa-rs (native Rust implementation)

use super::whatsapp_storage::RusqliteStore;
use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::select;
use waproto::whatsapp::device_props::PlatformType;
use zeroclaw_api::channel::{Channel, ChannelConversationScope, ChannelMessage, SendMessage};
#[cfg(feature = "whatsapp-web")]
use zeroclaw_api::media::MediaAttachment;
use zeroclaw_runtime::i18n;

#[cfg(feature = "whatsapp-web")]
pub struct WhatsAppWebChannel {
    /// Session database path
    session_path: String,
    /// Phone number for pair code linking (optional)
    pair_phone: Option<String>,
    /// Custom pair code (optional)
    pair_code: Option<String>,
    /// Override WebSocket URL (test / proxy setups). Sourced from
    /// `[whatsapp.ws_url]` — replaces the legacy `WHATSAPP_WS_URL` env-var
    /// read.
    ws_url: Option<String>,
    /// The alias key under `[channels.whatsapp.<alias>]` this handle is
    /// bound to. Used to scope peer-group writes and resolver lookups.
    alias: String,
    /// Resolves inbound external peers from canonical state at message-time.
    /// No cache (see AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH").
    peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// When true, only respond to messages that @-mention the bot in groups
    mention_only: bool,
    /// When true, allowed unaddressed group messages become context-only
    /// history entries instead of being dropped.
    passive_group_context: bool,
    /// Bot phone number (digits only), resolved from pair_phone or device identity at runtime
    bot_phone: Arc<Mutex<Option<String>>>,
    /// Bot LID number (digits only), resolved from device identity at runtime
    bot_lid: Arc<Mutex<Option<String>>>,
    /// Usage mode (business vs personal policy filtering)
    mode: zeroclaw_config::schema::WhatsAppWebMode,
    /// DM policy when mode = personal
    dm_policy: zeroclaw_config::schema::WhatsAppChatPolicy,
    /// Group policy when mode = personal
    group_policy: zeroclaw_config::schema::WhatsAppChatPolicy,
    /// Whether to always respond in self-chat when mode = personal
    self_chat_mode: bool,
    /// Bot handle for shutdown.
    /// whatsapp-rust 0.6: `Bot::run()` now returns `BotHandle` (a Future + abort)
    /// rather than a tokio JoinHandle directly (oxidezap/whatsapp-rust BotHandle wrapper).
    bot_handle: Arc<Mutex<Option<whatsapp_rust::bot::BotHandle>>>,
    /// Client handle for sending messages and typing indicators
    client: Arc<Mutex<Option<Arc<whatsapp_rust::Client>>>>,
    /// Message sender channel
    tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ChannelMessage>>>>,
    /// Voice transcription (STT) config
    transcription: Option<zeroclaw_config::schema::TranscriptionConfig>,
    transcription_manager: Option<std::sync::Arc<super::transcription::TranscriptionManager>>,
    /// Text-to-speech runtime for voice replies (built from
    /// `tts_providers.<type>.<alias>`).
    tts_manager: Option<Arc<super::tts::TtsManager>>,
    /// Chats awaiting a voice reply — maps chat JID to the latest substantive
    /// reply text. A background task debounces and sends the voice note after
    /// the agent finishes its turn (no new send() for 3 seconds).
    pending_voice:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>>,
    /// Chats whose last incoming message was a voice note.
    voice_chats: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Compiled mention patterns for DM mention gating.
    dm_mention_patterns: Arc<Vec<regex::Regex>>,
    /// Compiled mention patterns for group-chat mention gating.
    /// When non-empty, only group messages matching at least one pattern are
    /// processed; matched fragments are stripped from the forwarded content.
    group_mention_patterns: Arc<Vec<regex::Regex>>,
    /// Resolved channel workspace root used to bound outbound local media
    /// marker reads. The source of truth remains
    /// `Config::channel_workspace_dir("whatsapp.<alias>")`; this is the
    /// runtime trust boundary for file delivery.
    workspace_dir: Option<PathBuf>,
    /// Resolves allowed group chats from canonical config at message-time.
    /// Empty = all groups permitted. Direct messages bypass.
    allowed_groups_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// Optional pairing-persist handle to the canonical shared `Config`.
    /// `None` in tests; `Some` in the long-running daemon, wired via
    /// `.with_persistence(config)`. Same contract as WeChat's handle: on
    /// connect, the linked account is persisted into `peer_groups` through
    /// `crate::identity_persist` (no channel-local allowlist cache).
    persist: Option<Arc<parking_lot::RwLock<zeroclaw_config::schema::Config>>>,
}

impl WhatsAppWebChannel {
    #[cfg(feature = "whatsapp-web")]
    pub fn new(
        config: &zeroclaw_config::schema::WhatsAppConfig,
        alias: impl Into<String>,
        peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        allowed_groups_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    ) -> Self {
        let session_path = config.session_path.clone().unwrap_or_default();
        let pair_phone = config.pair_phone.clone();
        let pair_code = config.pair_code.clone();
        let ws_url = config.ws_url.clone();
        let mention_only = config.mention_only;
        let passive_group_context = config.passive_group_context;
        let mode = config.mode.clone();
        let dm_policy = config.dm_policy.clone();
        let group_policy = config.group_policy.clone();
        let self_chat_mode = config.self_chat_mode;

        // Seed bot_phone from pair_phone (digits only)
        let bot_phone = pair_phone
            .as_ref()
            .map(|p| p.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
            .filter(|digits| !digits.is_empty());

        if mention_only && bot_phone.is_none() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "mention_only enabled but pair_phone not set. \
                Bot identity will be resolved after connection. Group messages \
                will be skipped until identity is known."
            );
        }

        Self {
            session_path,
            pair_phone,
            pair_code,
            ws_url,
            alias: alias.into(),
            peer_resolver,
            mention_only,
            passive_group_context,
            bot_phone: Arc::new(Mutex::new(bot_phone)),
            bot_lid: Arc::new(Mutex::new(None)),
            mode,
            dm_policy,
            group_policy,
            self_chat_mode,
            allowed_groups_resolver,
            bot_handle: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            tx: Arc::new(Mutex::new(None)),
            transcription: None,
            transcription_manager: None,
            tts_manager: None,
            pending_voice: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            voice_chats: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            dm_mention_patterns: Arc::new(Vec::new()),
            group_mention_patterns: Arc::new(Vec::new()),
            workspace_dir: None,
            persist: None,
        }
    }

    /// Return the alias under `[channels.whatsapp.<alias>]` that this
    /// channel handle is bound to.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Wire the shared Config handle so a completed pairing can persist the
    /// linked account into `peer_groups` and save — the same contract as
    /// `WeChatChannel::with_persistence`. The long-running daemon sets this
    /// from the orchestrator; tests and one-shot callers leave it unset
    /// (pairing works at runtime, doesn't persist).
    #[cfg(feature = "whatsapp-web")]
    pub fn with_persistence(
        mut self,
        config: Arc<parking_lot::RwLock<zeroclaw_config::schema::Config>>,
    ) -> Self {
        self.persist = Some(config);
        self
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn with_workspace_dir(mut self, dir: PathBuf) -> Self {
        self.workspace_dir = Some(dir);
        self
    }

    /// Configure voice transcription (STT) for incoming voice notes.
    #[cfg(feature = "whatsapp-web")]
    pub fn with_transcription(
        mut self,
        config: zeroclaw_config::schema::TranscriptionConfig,
    ) -> Self {
        if !config.enabled {
            return self;
        }
        match super::transcription::TranscriptionManager::new(&config) {
            Ok(m) => {
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

    #[cfg(feature = "whatsapp-web")]
    pub fn with_tts(mut self, config: &zeroclaw_config::schema::Config) -> Self {
        if config.tts.enabled {
            let owner = config.agent_for_channel(&format!("whatsapp.{}", self.alias));
            match super::tts::TtsManager::from_config_for_agent(config, owner) {
                Ok(m) => self.tts_manager = Some(Arc::new(m)),
                Err(e) => ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "TTS disabled"
                ),
            }
        }
        self
    }

    /// Set mention patterns for DM mention gating.
    /// Each pattern string is compiled as a case-insensitive regex.
    /// Invalid patterns are logged and skipped.
    #[cfg(feature = "whatsapp-web")]
    pub fn with_dm_mention_patterns(mut self, patterns: Vec<String>) -> Self {
        self.dm_mention_patterns = Arc::new(
            super::whatsapp::WhatsAppChannel::compile_mention_patterns(&patterns),
        );
        self
    }

    /// Set mention patterns for group-chat mention gating.
    /// Each pattern string is compiled as a case-insensitive regex.
    /// Invalid patterns are logged and skipped.
    #[cfg(feature = "whatsapp-web")]
    pub fn with_group_mention_patterns(mut self, patterns: Vec<String>) -> Self {
        self.group_mention_patterns = Arc::new(
            super::whatsapp::WhatsAppChannel::compile_mention_patterns(&patterns),
        );
        self
    }

    /// Check if a phone number is allowed (E.164 format: +1234567890)
    #[cfg(feature = "whatsapp-web")]
    fn is_number_allowed(&self, phone: &str) -> bool {
        let peers = (self.peer_resolver)();
        Self::is_number_allowed_for_list(&peers, phone)
    }

    #[cfg(feature = "whatsapp-web")]
    fn is_number_allowed_for_list(allowed_numbers: &[String], phone: &str) -> bool {
        // This channel historically accepted a surrounding-whitespace wildcard
        // (`entry.trim() == "*"`), which is broader than the shared helper's
        // exact `"*"` check, so keep that pre-check here.
        if allowed_numbers.iter().any(|entry| entry.trim() == "*") {
            return true;
        }
        crate::allowlist::is_user_allowed_by(allowed_numbers, phone, |entry, phone| {
            match (
                Self::normalize_phone_token(entry),
                Self::normalize_phone_token(phone),
            ) {
                (Some(entry_norm), Some(phone_norm)) => entry_norm == phone_norm,
                _ => false,
            }
        })
    }

    /// Normalize a phone-like token to canonical E.164 (`+<digits>`).
    /// Accepts raw numbers, `+` numbers, and JIDs (uses the user part before `@`).
    #[cfg(feature = "whatsapp-web")]
    fn normalize_phone_token(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }

        let user_part = trimmed
            .split_once('@')
            .map(|(user, _)| user)
            .unwrap_or(trimmed)
            .trim();

        let digits: String = user_part.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            None
        } else {
            Some(format!("+{digits}"))
        }
    }

    #[cfg(feature = "whatsapp-web")]
    fn lid_rejection_diagnostic(
        sender: &wacore_binary::jid::Jid,
        mapped_phone: Option<&str>,
    ) -> String {
        if !sender.is_lid() {
            return String::new();
        }
        if mapped_phone.is_none() {
            format!(
                " (LID→phone resolution returned None for sender {sender}; \
                 allowlist phone-number entries cannot match. Workaround: \
                 add the LID-form (+{}) to allowed_numbers, or wait for the \
                 in-memory LID cache to populate for this contact.)",
                sender.user
            )
        } else {
            " (sender is LID; resolved phone did not match any allowlist entry)".to_string()
        }
    }

    /// Build normalized sender candidates from sender JID, optional alt JID, and optional LID->PN mapping.
    #[cfg(feature = "whatsapp-web")]
    fn sender_phone_candidates(
        sender: &wacore_binary::jid::Jid,
        sender_alt: Option<&wacore_binary::jid::Jid>,
        mapped_phone: Option<&str>,
    ) -> Vec<String> {
        let mut candidates = Vec::new();

        let mut add_candidate = |candidate: Option<String>| {
            if let Some(candidate) = candidate
                && !candidates.iter().any(|existing| existing == &candidate)
            {
                candidates.push(candidate);
            }
        };

        add_candidate(Self::normalize_phone_token(&sender.to_string()));
        if let Some(alt) = sender_alt {
            add_candidate(Self::normalize_phone_token(&alt.to_string()));
        }
        if let Some(mapped_phone) = mapped_phone {
            add_candidate(Self::normalize_phone_token(mapped_phone));
        }

        candidates
    }

    #[cfg(feature = "whatsapp-web")]
    fn compute_reply_target(chat_jid: &str) -> String {
        // Pass through unchanged - library handles LID resolution internally
        chat_jid.to_string()
    }

    /// Resolve an outbound recipient. With whatsapp-rust 0.6+ and
    /// LID JIDs are handled internally by the library, so we pass through unchanged.
    #[cfg(feature = "whatsapp-web")]
    fn resolve_outbound_recipient(recipient: &str) -> String {
        // Pass through unchanged - library handles LID resolution internally
        recipient.trim().to_string()
    }

    /// Normalize phone number to E.164 format
    #[cfg(feature = "whatsapp-web")]
    fn normalize_phone(&self, phone: &str) -> String {
        if let Some(normalized) = Self::normalize_phone_token(phone) {
            return normalized;
        }

        let trimmed = phone.trim();
        let user_part = trimmed
            .split_once('@')
            .map(|(user, _)| user)
            .unwrap_or(trimmed);
        let normalized_user = user_part.trim_start_matches('+');
        format!("+{normalized_user}")
    }

    /// Whether the recipient string is a WhatsApp JID (contains a domain suffix).
    #[cfg(feature = "whatsapp-web")]
    fn is_jid(recipient: &str) -> bool {
        recipient.trim().contains('@')
    }

    /// Render a WhatsApp pairing QR payload into terminal-friendly text.
    #[cfg(feature = "whatsapp-web")]
    fn render_pairing_qr(code: &str) -> Result<String> {
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
                "Failed to encode WhatsApp Web QR payload"
            );
            anyhow::Error::msg(format!("Failed to encode WhatsApp Web QR payload: {err}"))
        })?;

        Ok(qr
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build())
    }

    #[cfg(feature = "whatsapp-web")]
    fn recipient_to_jid(&self, recipient: &str) -> Result<wacore_binary::jid::Jid> {
        let trimmed = recipient.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Recipient cannot be empty");
        }

        if trimmed.contains('@') {
            return trimmed.parse::<wacore_binary::jid::Jid>().map_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "trimmed": trimmed,
                            "error": format!("{}", e),
                        })),
                    "whatsapp_web: invalid JID"
                );
                anyhow::Error::msg(format!("Invalid WhatsApp JID `{trimmed}`: {e}"))
            });
        }

        let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            anyhow::bail!("Recipient `{trimmed}` does not contain a valid phone number");
        }

        Ok(wacore_binary::jid::Jid::pn(digits))
    }

    // ── Reconnect state-machine helpers (used by listen() and tested directly) ──

    /// Reconnect retry constants.
    const MAX_RETRIES: u32 = 10;
    const BASE_DELAY_SECS: u64 = 3;
    const MAX_DELAY_SECS: u64 = 300;

    /// Compute the exponential-backoff delay for a given 1-based attempt number.
    /// Doubles each attempt from `BASE_DELAY_SECS`, capped at `MAX_DELAY_SECS`.
    fn compute_retry_delay(attempt: u32) -> u64 {
        std::cmp::min(
            Self::BASE_DELAY_SECS.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1))),
            Self::MAX_DELAY_SECS,
        )
    }

    /// Determine whether session files should be purged.
    /// Returns `true` only when `Event::LoggedOut` was explicitly observed.
    fn should_purge_session(session_revoked: &std::sync::atomic::AtomicBool) -> bool {
        session_revoked.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record a reconnect attempt and return `(attempt_number, exceeded_max)`.
    fn record_retry(retry_count: &std::sync::atomic::AtomicU32) -> (u32, bool) {
        let attempts = retry_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        (attempts, attempts > Self::MAX_RETRIES)
    }

    /// Reset the retry counter (called on `Event::Connected`).
    fn reset_retry(retry_count: &std::sync::atomic::AtomicU32) {
        retry_count.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Expand `~` in a configured `session_path`. Single source of truth
    /// for the on-disk location — the run loop and the readiness probe
    /// must agree on the file they are looking at.
    fn expand_session_path(session_path: &str) -> String {
        shellexpand::tilde(session_path).to_string()
    }

    /// Channel-owned persisted-login probe: reports whether the session
    /// database at the configured `session_path` holds a device linked to a
    /// WhatsApp account (`device.pn` written by a completed QR pairing).
    /// Stricter than the run loop's resume check on purpose — a channel
    /// waiting for its QR scan persists an unregistered device row, which
    /// must not read as an authenticated login. Read-only; never creates
    /// the database or its sidecar files.
    pub fn has_persisted_session(session_path: &str) -> bool {
        if session_path.is_empty() {
            return false;
        }
        super::whatsapp_storage::persisted_device_exists(Self::expand_session_path(session_path))
    }

    /// Return the session file paths to remove (primary + WAL + SHM sidecars).
    fn session_file_paths(expanded_session_path: &str) -> [String; 3] {
        [
            expanded_session_path.to_string(),
            format!("{expanded_session_path}-wal"),
            format!("{expanded_session_path}-shm"),
        ]
    }

    /// Channel-owned relink hook: delete the persisted session so the next
    /// channel start finds no device and begins a fresh QR pairing.
    ///
    /// Removes the same triple the logged-out purge path removes —
    /// [`Self::session_file_paths`] is the single source of truth for both.
    /// Returns the paths actually removed; already absent files are not an
    /// error, so relinking an unpaired channel is a safe no-op that returns
    /// an empty list. Never creates the database.
    ///
    /// This only clears disk state. A currently running channel keeps its
    /// live connection until it is restarted; callers own scheduling that
    /// restart (e.g. a daemon reload).
    pub fn clear_persisted_session(session_path: &str) -> std::io::Result<Vec<String>> {
        let mut removed = Vec::new();
        if session_path.is_empty() {
            return Ok(removed);
        }
        let expanded = Self::expand_session_path(session_path);
        for path in Self::session_file_paths(&expanded) {
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    }

    /// Attempt to download and transcribe a WhatsApp voice note.
    /// Returns `None` if transcription is disabled, download fails, or
    /// transcription fails (all logged as warnings).
    #[cfg(feature = "whatsapp-web")]
    async fn try_transcribe_voice_note(
        client: &whatsapp_rust::Client,
        audio: &waproto::whatsapp::message::AudioMessage,
        transcription_config: Option<&zeroclaw_config::schema::TranscriptionConfig>,
        transcription_manager: Option<&super::transcription::TranscriptionManager>,
    ) -> Option<String> {
        let config = transcription_config?;
        let manager = transcription_manager?;

        // Enforce duration limit
        if let Some(seconds) = audio.seconds
            && u64::from(seconds) > config.max_duration_secs
        {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!(
                    "skipping voice note ({}s exceeds {}s limit)",
                    seconds, config.max_duration_secs
                )
            );
            return None;
        }

        // Download the encrypted audio
        use whatsapp_rust::download::Downloadable;
        let audio_data = match client.download(audio as &dyn Downloadable).await {
            Ok(data) => data,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "failed to download voice note"
                );
                return None;
            }
        };

        // Determine filename from mimetype for transcription API
        let file_name = match audio.mimetype.as_deref() {
            Some(m) if m.contains("opus") || m.contains("ogg") => "voice.ogg",
            Some(m) if m.contains("mp4") || m.contains("m4a") => "voice.m4a",
            Some(m) if m.contains("mpeg") || m.contains("mp3") => "voice.mp3",
            Some(m) if m.contains("webm") => "voice.webm",
            _ => "voice.ogg", // WhatsApp default
        };

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "transcribing voice note ({} bytes, file={})",
                audio_data.len(),
                file_name
            )
        );

        match manager.transcribe(&audio_data, file_name).await {
            Ok(text) if text.trim().is_empty() => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "voice transcription returned empty text, skipping"
                );
                None
            }
            Ok(text) => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!("voice note transcribed ({} chars)", text.len())
                );
                Some(text)
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "voice transcription failed"
                );
                None
            }
        }
    }

    #[cfg(feature = "whatsapp-web")]
    fn extract_context_info(
        msg: &waproto::whatsapp::Message,
    ) -> Option<&waproto::whatsapp::ContextInfo> {
        use wacore::proto_helpers::MessageExt;
        let base = msg.get_base_message();

        if let Some(ref ext) = base.extended_text_message
            && let Some(ref ctx) = ext.context_info
        {
            return Some(ctx);
        }
        if let Some(ref img) = base.image_message
            && let Some(ref ctx) = img.context_info
        {
            return Some(ctx);
        }
        if let Some(ref vid) = base.video_message
            && let Some(ref ctx) = vid.context_info
        {
            return Some(ctx);
        }
        if let Some(ref doc) = base.document_message
            && let Some(ref ctx) = doc.context_info
        {
            return Some(ctx);
        }
        if let Some(ref aud) = base.audio_message
            && let Some(ref ctx) = aud.context_info
        {
            return Some(ctx);
        }
        if let Some(ref stk) = base.sticker_message
            && let Some(ref ctx) = stk.context_info
        {
            return Some(ctx);
        }

        None
    }

    #[cfg(feature = "whatsapp-web")]
    fn extract_quoted_message(
        msg: &waproto::whatsapp::Message,
    ) -> Option<&waproto::whatsapp::Message> {
        Self::extract_context_info(msg).and_then(|ctx| ctx.quoted_message.as_deref())
    }

    #[cfg(feature = "whatsapp-web")]
    fn mime_extension(mime: &str, fallback: &str) -> String {
        let subtype = mime
            .split(';')
            .next()
            .and_then(|clean| clean.split_once('/').map(|(_, subtype)| subtype))
            .and_then(|subtype| subtype.split('+').next())
            .filter(|subtype| {
                !subtype.is_empty()
                    && subtype
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
            })
            .unwrap_or(fallback);

        match subtype {
            "jpeg" => "jpg".to_string(),
            "svg+xml" => "svg".to_string(),
            other => other.to_string(),
        }
    }

    #[cfg(feature = "whatsapp-web")]
    async fn push_downloaded_attachment(
        client: &whatsapp_rust::Client,
        downloadable: &dyn whatsapp_rust::download::Downloadable,
        file_name: String,
        mime_type: Option<String>,
        attachments: &mut Vec<MediaAttachment>,
    ) {
        let data = match client.download(downloadable).await {
            Ok(data) => data,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "file": file_name,
                            "error": format!("{}", e),
                        })),
                    "failed to download WhatsApp media attachment"
                );
                return;
            }
        };

        attachments.push(MediaAttachment {
            file_name,
            data,
            mime_type,
        });
    }

    #[cfg(feature = "whatsapp-web")]
    async fn collect_media_attachments(
        client: &whatsapp_rust::Client,
        msg: &waproto::whatsapp::Message,
        file_prefix: &str,
        include_audio: bool,
        attachments: &mut Vec<MediaAttachment>,
    ) {
        use wacore::proto_helpers::MessageExt;
        use whatsapp_rust::download::Downloadable;

        let base = msg.get_base_message();

        if let Some(ref image) = base.image_message {
            let mime = image
                .mimetype
                .clone()
                .unwrap_or_else(|| "image/jpeg".to_string());
            let file_name = format!(
                "{file_prefix}whatsapp-image.{}",
                Self::mime_extension(&mime, "jpg")
            );
            Self::push_downloaded_attachment(
                client,
                image.as_ref() as &dyn Downloadable,
                file_name,
                Some(mime),
                attachments,
            )
            .await;
        }

        if let Some(ref video) = base.video_message {
            let mime = video
                .mimetype
                .clone()
                .unwrap_or_else(|| "video/mp4".to_string());
            let file_name = format!(
                "{file_prefix}whatsapp-video.{}",
                Self::mime_extension(&mime, "mp4")
            );
            Self::push_downloaded_attachment(
                client,
                video.as_ref() as &dyn Downloadable,
                file_name,
                Some(mime),
                attachments,
            )
            .await;
        }

        if include_audio && let Some(ref audio) = base.audio_message {
            let mime = audio
                .mimetype
                .clone()
                .unwrap_or_else(|| "audio/ogg".to_string());
            let file_name = format!(
                "{file_prefix}whatsapp-audio.{}",
                Self::mime_extension(&mime, "ogg")
            );
            Self::push_downloaded_attachment(
                client,
                audio.as_ref() as &dyn Downloadable,
                file_name,
                Some(mime),
                attachments,
            )
            .await;
        }

        if let Some(ref sticker) = base.sticker_message {
            let mime = sticker
                .mimetype
                .clone()
                .unwrap_or_else(|| "image/webp".to_string());
            let file_name = format!(
                "{file_prefix}whatsapp-sticker.{}",
                Self::mime_extension(&mime, "webp")
            );
            Self::push_downloaded_attachment(
                client,
                sticker.as_ref() as &dyn Downloadable,
                file_name,
                Some(mime),
                attachments,
            )
            .await;
        }
    }

    #[cfg(feature = "whatsapp-web")]
    fn media_fallback_content(content: String, msg: &waproto::whatsapp::Message) -> String {
        if !content.is_empty() {
            return content;
        }

        use wacore::proto_helpers::MessageExt;
        let base = msg.get_base_message();

        if base.sticker_message.is_some() {
            return "[Sticker]".to_string();
        }
        if base.image_message.is_some() {
            return "[Image]".to_string();
        }
        if base.video_message.is_some() {
            return "[Video]".to_string();
        }
        if base.document_message.is_some() {
            return "[Document]".to_string();
        }
        if let Some(ref loc) = base.location_message {
            // Live locations are silently ignored — they stream
            // periodic updates and have no meaningful static content.
            if loc.is_live == Some(true) {
                return String::new();
            }
            let lat = match loc.degrees_latitude {
                Some(l) => l,
                None => return String::new(),
            };
            let lng = match loc.degrees_longitude {
                Some(l) => l,
                None => return String::new(),
            };
            return crate::util::format_location_content(lat, lng, loc.name.as_deref());
        }

        String::new()
    }

    #[cfg(feature = "whatsapp-web")]
    fn group_context_scope(
        passive_group_context: bool,
        is_group: bool,
    ) -> ChannelConversationScope {
        if passive_group_context && is_group {
            ChannelConversationScope::ReplyTarget
        } else {
            ChannelConversationScope::Sender
        }
    }

    #[cfg(feature = "whatsapp-web")]
    fn should_record_passive_group_context(
        passive_group_context: bool,
        is_group: bool,
        addressed_to_bot: bool,
    ) -> bool {
        passive_group_context && is_group && !addressed_to_bot
    }

    #[cfg(feature = "whatsapp-web")]
    async fn send_inbound_channel_message(
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
        alias: &str,
        sender: &str,
        reply_target: String,
        content: String,
        attachments: Vec<MediaAttachment>,
        passive_context: bool,
        conversation_scope: ChannelConversationScope,
    ) {
        if let Err(e) = tx
            .send(ChannelMessage {
                id: uuid::Uuid::new_v4().to_string(),
                channel: "whatsapp".to_string(),
                channel_alias: Some(alias.to_string()),
                sender: sender.to_string(),
                // Reply to the originating chat JID (DM or group), passed
                // through unchanged (library handles LID addressing internally).
                reply_target,
                content,
                timestamp: chrono::Utc::now().timestamp() as u64,
                thread_ts: None,
                interruption_scope_id: None,
                attachments,
                subject: None,
                internal_sop_event: None,
                passive_context,
                explicitly_addressed: false,
                conversation_scope,
            })
            .await
        {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "failed to send message to channel"
            );
        }
    }

    /// Synthesize text to speech and send as a WhatsApp voice note (static version for spawned tasks).
    #[cfg(feature = "whatsapp-web")]
    async fn synthesize_voice_static(
        client: &whatsapp_rust::Client,
        to: &wacore_binary::jid::Jid,
        text: &str,
        tts_manager: &super::tts::TtsManager,
    ) -> Result<()> {
        let audio_bytes = tts_manager.synthesize_opus(text).await?;
        let audio_len = audio_bytes.len();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("TTS: synthesized {} bytes of audio", audio_len)
        );

        if audio_bytes.is_empty() {
            anyhow::bail!("TTS returned empty audio");
        }

        use wacore::download::MediaType;
        use whatsapp_rust::upload::UploadOptions;
        let upload = client
            .upload(audio_bytes, MediaType::Audio, UploadOptions::default())
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Failed to upload TTS audio"
                );
                anyhow::Error::msg(format!("Failed to upload TTS audio: {e}"))
            })?;

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "TTS: uploaded audio (url_len={}, file_length={})",
                upload.url.len(),
                upload.file_length
            )
        );

        // Estimate duration from file size: Opus at ~32 kbps → bytes / 4000 ≈ seconds
        #[allow(clippy::cast_possible_truncation)]
        let estimated_seconds = std::cmp::max(1, (upload.file_length / 4000) as u32);

        // whatsapp-rust 0.6: UploadResponse cryptographic fields became
        // `[u8; 32]` for type safety. Pull the Vec<u8> copies before
        // consuming the strings so the partial-move on `upload.direct_path`
        // doesn't bite.
        let media_key = upload.media_key_vec();
        let file_enc_sha256 = upload.file_enc_sha256_vec();
        let file_sha256 = upload.file_sha256_vec();
        let voice_msg = waproto::whatsapp::Message {
            audio_message: Some(Box::new(waproto::whatsapp::message::AudioMessage {
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(media_key),
                file_enc_sha256: Some(file_enc_sha256),
                file_sha256: Some(file_sha256),
                file_length: Some(upload.file_length),
                mimetype: Some("audio/ogg; codecs=opus".to_string()),
                ptt: Some(true),
                seconds: Some(estimated_seconds),
                ..Default::default()
            })),
            ..Default::default()
        };

        Box::pin(client.send_message(to.clone(), voice_msg))
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Failed to send voice note"
                );
                anyhow::Error::msg(format!("Failed to send voice note: {e}"))
            })?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "TTS: sent voice note ({} bytes, ~{}s)",
                audio_len, estimated_seconds
            )
        );
        Ok(())
    }

    #[cfg(feature = "whatsapp-web")]
    async fn send_media_marker(
        client: &whatsapp_rust::Client,
        to: &wacore_binary::jid::Jid,
        marker: &WhatsAppMediaMarker,
        path: &Path,
    ) -> Result<()> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read WhatsApp marker target {}", path.display()))?;
        if bytes.is_empty() {
            anyhow::bail!("WhatsApp marker target {} is empty", path.display());
        }

        let media_type = marker.kind.media_type();
        let mime = marker.kind.mime_for_path(path);

        use whatsapp_rust::upload::UploadOptions;
        let upload = client
            .upload(bytes, media_type, UploadOptions::default())
            .await
            .map_err(|e| anyhow::Error::msg(format!("WhatsApp media upload failed: {e}")))?;

        let media_key = upload.media_key_vec();
        let file_enc_sha256 = upload.file_enc_sha256_vec();
        let file_sha256 = upload.file_sha256_vec();
        let outgoing = match marker.kind {
            WhatsAppMediaKind::Image => waproto::whatsapp::Message {
                image_message: Some(Box::new(waproto::whatsapp::message::ImageMessage {
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(media_key),
                    file_enc_sha256: Some(file_enc_sha256),
                    file_sha256: Some(file_sha256),
                    file_length: Some(upload.file_length),
                    mimetype: Some(mime),
                    ..Default::default()
                })),
                ..Default::default()
            },
            WhatsAppMediaKind::Video => waproto::whatsapp::Message {
                video_message: Some(Box::new(waproto::whatsapp::message::VideoMessage {
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(media_key),
                    file_enc_sha256: Some(file_enc_sha256),
                    file_sha256: Some(file_sha256),
                    file_length: Some(upload.file_length),
                    mimetype: Some(mime),
                    ..Default::default()
                })),
                ..Default::default()
            },
            WhatsAppMediaKind::Audio | WhatsAppMediaKind::Voice => {
                #[allow(clippy::cast_possible_truncation)]
                let estimated_seconds = std::cmp::max(1, (upload.file_length / 4000) as u32);
                waproto::whatsapp::Message {
                    audio_message: Some(Box::new(waproto::whatsapp::message::AudioMessage {
                        url: Some(upload.url),
                        direct_path: Some(upload.direct_path),
                        media_key: Some(media_key),
                        file_enc_sha256: Some(file_enc_sha256),
                        file_sha256: Some(file_sha256),
                        file_length: Some(upload.file_length),
                        mimetype: Some(mime),
                        ptt: Some(matches!(marker.kind, WhatsAppMediaKind::Voice)),
                        seconds: Some(estimated_seconds),
                        ..Default::default()
                    })),
                    ..Default::default()
                }
            }
            WhatsAppMediaKind::Document => {
                let file_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("attachment")
                    .to_string();
                waproto::whatsapp::Message {
                    document_message: Some(Box::new(waproto::whatsapp::message::DocumentMessage {
                        url: Some(upload.url),
                        direct_path: Some(upload.direct_path),
                        media_key: Some(media_key),
                        file_enc_sha256: Some(file_enc_sha256),
                        file_sha256: Some(file_sha256),
                        file_length: Some(upload.file_length),
                        mimetype: Some(mime),
                        file_name: Some(file_name.clone()),
                        title: Some(file_name),
                        ..Default::default()
                    })),
                    ..Default::default()
                }
            }
        };

        Box::pin(client.send_message(to.clone(), outgoing))
            .await
            .map_err(|e| anyhow::Error::msg(format!("WhatsApp media send failed: {e}")))?;
        Ok(())
    }

    /// Send a native location pin. No file read or media upload is involved —
    /// the coordinates and labels travel inline in a `LocationMessage`.
    #[cfg(feature = "whatsapp-web")]
    async fn send_location(
        client: &whatsapp_rust::Client,
        to: &wacore_binary::jid::Jid,
        loc: &WhatsAppLocation,
    ) -> Result<()> {
        let outgoing = waproto::whatsapp::Message {
            location_message: Some(Box::new(waproto::whatsapp::message::LocationMessage {
                degrees_latitude: Some(loc.lat),
                degrees_longitude: Some(loc.lng),
                name: loc.name.clone(),
                address: loc.address.clone(),
                ..Default::default()
            })),
            ..Default::default()
        };
        Box::pin(client.send_message(to.clone(), outgoing))
            .await
            .map_err(|e| anyhow::Error::msg(format!("WhatsApp location send failed: {e}")))?;
        Ok(())
    }

    // ── Mention detection helpers (used when mention_only is enabled) ──

    /// Extract digits from a JID string (e.g. "919211916069@s.whatsapp.net" -> "919211916069").
    #[cfg(feature = "whatsapp-web")]
    fn jid_digits(jid: &str) -> String {
        let user_part = jid.split_once('@').map(|(u, _)| u).unwrap_or(jid);
        let user_part = user_part
            .split_once(':')
            .map(|(u, _)| u)
            .unwrap_or(user_part);
        user_part.chars().filter(|c| c.is_ascii_digit()).collect()
    }

    #[cfg(feature = "whatsapp-web")]
    fn store_jid_digits(slot: &Arc<Mutex<Option<String>>>, jid: &str) -> Option<String> {
        let digits = Self::jid_digits(jid);
        if digits.is_empty() {
            None
        } else {
            *slot.lock() = Some(digits.clone());
            Some(digits)
        }
    }

    #[cfg(feature = "whatsapp-web")]
    fn jid_matches_bot(jid: &str, bot_phone: &str, bot_lid: Option<&str>) -> bool {
        let digits = Self::jid_digits(jid);
        !digits.is_empty()
            && ((!bot_phone.is_empty() && digits == bot_phone)
                || bot_lid.is_some_and(|lid| !lid.is_empty() && digits == lid))
    }

    /// Extract mentioned JIDs from the base (unwrapped) message's context_info.
    #[cfg(feature = "whatsapp-web")]
    fn extract_mentioned_jids(msg: &waproto::whatsapp::Message) -> Vec<String> {
        Self::extract_context_info(msg)
            .map(|ctx| ctx.mentioned_jid.clone())
            .unwrap_or_default()
    }

    /// Check whether the bot is mentioned -- either structurally or via text fallback.
    #[cfg(feature = "whatsapp-web")]
    fn contains_bot_mention(
        text: &str,
        mentioned_jids: &[String],
        bot_phone: &str,
        bot_lid: Option<&str>,
    ) -> bool {
        // 1. Structured: check if any mentioned_jid's digits match the bot's phone or LID digits
        for jid in mentioned_jids {
            if Self::jid_matches_bot(jid, bot_phone, bot_lid) {
                return true;
            }
        }

        // 2. Text fallback: word-boundary-aware match for @<bot_digits>.
        //    Scan all occurrences -- an earlier prefix false-match must not mask a later real mention.
        fn has_text_mention(text: &str, digits: &str) -> bool {
            if digits.is_empty() {
                return false;
            }

            let pattern = format!("@{digits}");
            let mut search_from = 0;
            while let Some(rel_pos) = text[search_from..].find(&pattern) {
                let pos = search_from + rel_pos;
                let after_idx = pos + pattern.len();
                let leading_ok = pos == 0
                    || text[..pos]
                        .chars()
                        .next_back()
                        .is_none_or(|ch| !ch.is_ascii_alphanumeric());
                let trailing_ok = text[after_idx..]
                    .chars()
                    .next()
                    .is_none_or(|ch| !ch.is_ascii_digit());
                if leading_ok && trailing_ok {
                    return true;
                }
                search_from = after_idx;
            }
            false
        }

        has_text_mention(text, bot_phone) || bot_lid.is_some_and(|lid| has_text_mention(text, lid))
    }

    /// Extract the author JID of the message quoted by a reply.
    #[cfg(feature = "whatsapp-web")]
    fn extract_reply_participant(msg: &waproto::whatsapp::Message) -> Option<&str> {
        Self::extract_context_info(msg).and_then(|ctx| ctx.participant.as_deref())
    }

    #[cfg(feature = "whatsapp-web")]
    fn is_reply_to_bot(
        msg: &waproto::whatsapp::Message,
        bot_phone: &str,
        bot_lid: Option<&str>,
    ) -> bool {
        Self::extract_reply_participant(msg)
            .is_some_and(|participant| Self::jid_matches_bot(participant, bot_phone, bot_lid))
    }

    #[cfg(feature = "whatsapp-web")]
    fn is_message_addressed_to_bot(
        msg: &waproto::whatsapp::Message,
        text: &str,
        bot_phone: &str,
        bot_lid: Option<&str>,
    ) -> bool {
        let mentioned_jids = Self::extract_mentioned_jids(msg);
        Self::contains_bot_mention(text, &mentioned_jids, bot_phone, bot_lid)
            || Self::is_reply_to_bot(msg, bot_phone, bot_lid)
    }
}

#[cfg(feature = "whatsapp-web")]
fn fromme_outside_self_chat_is_operator_trigger(
    is_group: bool,
    dm_mention_patterns: &[regex::Regex],
    group_mention_patterns: &[regex::Regex],
    text: &str,
) -> bool {
    let applicable = if is_group {
        group_mention_patterns
    } else {
        dm_mention_patterns
    };
    if applicable.is_empty() {
        return false;
    }
    super::whatsapp::WhatsAppChannel::text_matches_patterns(applicable, text)
}

#[cfg(feature = "whatsapp-web")]
fn is_group_chat_allowed(chat_jid: &str, allowed_groups: &[String]) -> bool {
    if allowed_groups.is_empty() {
        return true;
    }
    let chat_user = chat_jid
        .split_once('@')
        .map(|(user, _)| user)
        .unwrap_or(chat_jid);
    allowed_groups.iter().any(|entry| {
        let entry = entry.trim();
        !entry.is_empty() && (entry == chat_jid || entry == chat_user)
    })
}

#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhatsAppMediaKind {
    Image,
    Document,
    Video,
    Audio,
    Voice,
}

#[cfg(feature = "whatsapp-web")]
impl WhatsAppMediaKind {
    fn from_marker(kind: &str) -> Option<Self> {
        match kind {
            "IMAGE" | "PHOTO" => Some(Self::Image),
            "DOCUMENT" | "FILE" => Some(Self::Document),
            "VIDEO" => Some(Self::Video),
            "AUDIO" => Some(Self::Audio),
            "VOICE" => Some(Self::Voice),
            _ => None,
        }
    }

    fn media_type(self) -> wacore::download::MediaType {
        match self {
            Self::Image => wacore::download::MediaType::Image,
            Self::Document => wacore::download::MediaType::Document,
            Self::Video => wacore::download::MediaType::Video,
            Self::Audio | Self::Voice => wacore::download::MediaType::Audio,
        }
    }

    fn mime_for_path(self, path: &Path) -> String {
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(self, Self::Voice) && matches!(ext.as_deref(), Some("ogg" | "oga" | "opus")) {
            return "audio/ogg; codecs=opus".to_string();
        }
        match ext.as_deref() {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            Some("mp4") => "video/mp4",
            Some("mov") => "video/quicktime",
            Some("mkv") => "video/x-matroska",
            Some("avi") => "video/x-msvideo",
            Some("webm") => "video/webm",
            Some("mp3") => "audio/mpeg",
            Some("m4a") => "audio/mp4",
            Some("wav") => "audio/wav",
            Some("flac") => "audio/flac",
            Some("ogg" | "oga") => "audio/ogg",
            Some("opus") => "audio/opus",
            Some("pdf") => "application/pdf",
            Some("doc") => "application/msword",
            Some("docx") => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            Some("xls") => "application/vnd.ms-excel",
            Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("csv") => "text/csv",
            Some("txt") => "text/plain",
            _ => "application/octet-stream",
        }
        .to_string()
    }
}

#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct WhatsAppMediaMarker {
    kind: WhatsAppMediaKind,
    /// Path-like target resolved against the workspace before upload.
    target: String,
}

#[cfg(feature = "whatsapp-web")]
use crate::util::WhatsAppLocation;

#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, PartialEq)]
enum WhatsAppMarker {
    Media(WhatsAppMediaMarker),
    Location(String),
}

#[cfg(feature = "whatsapp-web")]
impl WhatsAppMarker {
    fn from_shared_marker(kind: String, target: String) -> Option<Self> {
        if kind.eq_ignore_ascii_case("LOCATION") {
            return Some(Self::Location(target));
        }
        let kind = WhatsAppMediaKind::from_marker(&kind)?;
        Some(Self::Media(WhatsAppMediaMarker { kind, target }))
    }

    /// Short label for structured logs.
    fn kind_label(&self) -> String {
        match self {
            Self::Media(m) => format!("{:?}", m.kind),
            Self::Location(_) => "Location".to_string(),
        }
    }
}

#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhatsAppMarkerFailure {
    Refused,
    Failed,
}

#[cfg(feature = "whatsapp-web")]
#[derive(Debug)]
enum WhatsAppMarkerError {
    Refused(anyhow::Error),
    Failed(anyhow::Error),
}

#[cfg(feature = "whatsapp-web")]
impl std::fmt::Display for WhatsAppMarkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(err) | Self::Failed(err) => write!(f, "{err}"),
        }
    }
}

#[cfg(feature = "whatsapp-web")]
impl WhatsAppMarkerError {
    fn kind(&self) -> WhatsAppMarkerFailure {
        match self {
            Self::Refused(_) => WhatsAppMarkerFailure::Refused,
            Self::Failed(_) => WhatsAppMarkerFailure::Failed,
        }
    }
}

#[cfg(feature = "whatsapp-web")]
fn validate_whatsapp_marker_target(
    target: &str,
    workspace_dir: Option<&Path>,
) -> std::result::Result<PathBuf, WhatsAppMarkerError> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Err(WhatsAppMarkerError::Refused(anyhow::Error::msg(
            "WhatsApp Web media markers currently accept local workspace files only",
        )));
    }
    let disallowed_scheme = if target.starts_with("data:") {
        Some("data")
    } else if target.starts_with("file:") {
        Some("file")
    } else if target.contains("://") {
        Some(target.split("://").next().unwrap_or("?"))
    } else {
        None
    };
    if let Some(scheme) = disallowed_scheme {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"scheme": scheme})),
            "whatsapp-web: marker target uses disallowed scheme"
        );
        return Err(WhatsAppMarkerError::Refused(anyhow::Error::msg(
            "WhatsApp Web marker target uses a disallowed scheme",
        )));
    }

    let workspace = workspace_dir.ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"reason": "no_workspace_dir"})),
            "whatsapp-web: local marker target has no workspace_dir"
        );
        WhatsAppMarkerError::Refused(anyhow::Error::msg(
            "WhatsApp Web channel was started without a workspace_dir",
        ))
    })?;
    let workspace_canon = std::fs::canonicalize(workspace)
        .with_context(|| format!("canonicalize workspace {}", workspace.display()))
        .map_err(WhatsAppMarkerError::Refused)?;
    let target_path = Path::new(target);
    let absolute = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        workspace_canon.join(target_path)
    };
    let target_canon = match std::fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"reason": "not_found"})),
                "whatsapp-web: marker target not found on disk"
            );
            return Err(WhatsAppMarkerError::Failed(anyhow::Error::msg(
                "WhatsApp Web marker target not found on disk",
            )));
        }
        Err(err) => {
            return Err(WhatsAppMarkerError::Refused(
                anyhow::Error::from(err).context("canonicalize WhatsApp marker target"),
            ));
        }
    };

    if !target_canon.starts_with(&workspace_canon) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"reason": "outside_workspace"})),
            "whatsapp-web: marker target escapes workspace_dir"
        );
        return Err(WhatsAppMarkerError::Refused(anyhow::Error::msg(
            "WhatsApp Web marker target resolves outside workspace_dir",
        )));
    }
    Ok(target_canon)
}

#[cfg(feature = "whatsapp-web")]
fn validate_whatsapp_location_target(
    target: &str,
) -> std::result::Result<WhatsAppLocation, WhatsAppMarkerError> {
    WhatsAppLocation::parse(target).ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"reason": "invalid_location"})),
            "whatsapp-web: location marker target is malformed or outside WGS84 range"
        );
        WhatsAppMarkerError::Refused(anyhow::Error::msg(
            "WhatsApp Web location marker must be `lat,lng[,name[,address]]` with in-range WGS84 coordinates",
        ))
    })
}

#[cfg(feature = "whatsapp-web")]
fn whatsapp_delivery_failure_note(failure_count: usize) -> Option<String> {
    if failure_count == 0 {
        return None;
    }
    let count = failure_count.to_string();
    let key = if failure_count == 1 {
        "channel-whatsapp-web-delivery-failure-note-one"
    } else {
        "channel-whatsapp-web-delivery-failure-note-many"
    };
    Some(i18n::get_required_cli_string_with_args(
        key,
        &[("count", count.as_str())],
    ))
}

#[cfg(feature = "whatsapp-web")]
impl ::zeroclaw_api::attribution::Attributable for WhatsAppWebChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(
            ::zeroclaw_api::attribution::ChannelKind::WhatsappWeb,
        )
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl Channel for WhatsAppWebChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        // Validate recipient allowlist only for direct phone-number targets.
        if !Self::is_jid(&message.recipient) {
            let normalized = self.normalize_phone(&message.recipient);
            if !self.is_number_allowed(&normalized) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!("recipient {} not in allowed list", message.recipient)
                );
                return Ok(());
            }
        }

        let deliverable_recipient = Self::resolve_outbound_recipient(&message.recipient);
        let to = self.recipient_to_jid(&deliverable_recipient)?;
        let raw_content = if message.content.contains("<function_calls")
            || message.content.contains("</function_calls")
            || message.content.contains("<tool_call")
            || message.content.contains("</tool_call")
            || message.content.contains("<tool_calls")
            || message.content.contains("</tool_calls")
        {
            crate::util::strip_tool_call_tags(&message.content)
        } else {
            message.content.clone()
        };
        let (mut text_content, raw_markers) = if raw_content.contains('[')
            && raw_content.contains(':')
            && raw_content.contains(']')
        {
            let (cleaned, raw_markers) = super::util::parse_attachment_markers(&raw_content);
            if raw_markers.is_empty() {
                (raw_content, raw_markers)
            } else {
                (cleaned, raw_markers)
            }
        } else {
            (raw_content, Vec::new())
        };
        let markers = raw_markers
            .into_iter()
            .filter_map(|(kind, target)| WhatsAppMarker::from_shared_marker(kind, target))
            .collect::<Vec<_>>();

        // Voice chat mode: send text normally AND queue a voice note of the
        // final answer. Only substantive messages (not tool outputs) are queued.
        // A debounce task waits 10s after the last substantive message, then
        // sends ONE voice note. Text in → text out. Voice in → text + voice out.
        let is_voice_chat = self
            .voice_chats
            .lock()
            .map(|vs| vs.contains(&message.recipient))
            .unwrap_or(false);

        if is_voice_chat && self.tts_manager.is_some() {
            let content = &text_content;
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

            if is_substantive {
                if let Ok(mut pv) = self.pending_voice.lock() {
                    pv.insert(
                        message.recipient.clone(),
                        (content.clone(), std::time::Instant::now()),
                    );
                }

                let pending = self.pending_voice.clone();
                let voice_chats = self.voice_chats.clone();
                let client_clone = client.clone();
                let to_clone = to.clone();
                let recipient = message.recipient.clone();
                let tts_manager = self.tts_manager.clone().unwrap();
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
                        if let Ok(mut vc) = voice_chats.lock() {
                            vc.remove(&recipient);
                        }
                        match Box::pin(WhatsAppWebChannel::synthesize_voice_static(
                            &client_clone,
                            &to_clone,
                            &text,
                            &tts_manager,
                        ))
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
                                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                    "TTS voice reply failed"
                                );
                            }
                        }
                    }
                });
            }
            // Fall through to send text normally (voice chat gets BOTH)
        }

        let mut delivered_markers = 0usize;
        let mut failed_marker_count = 0usize;
        for marker in &markers {
            // Location markers carry inline data (lat,lng,...) and skip the
            // workspace file validator, but the coordinates must still parse
            // and land in WGS84 range; media markers must resolve to a real
            // file inside the workspace before upload.
            let result = match marker {
                WhatsAppMarker::Location(target) => {
                    match validate_whatsapp_location_target(target) {
                        Ok(loc) => Self::send_location(&client, &to, &loc).await,
                        Err(err) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "kind": "Location",
                                    "reason": "invalid coordinates",
                                    "error": err.to_string(),
                                })),
                                "whatsapp-web: dropping unresolved outbound attachment marker"
                            );
                            failed_marker_count += 1;
                            continue;
                        }
                    }
                }
                WhatsAppMarker::Media(media) => {
                    let target = match validate_whatsapp_marker_target(
                        &media.target,
                        self.workspace_dir.as_deref(),
                    ) {
                        Ok(path) => path,
                        Err(err) => {
                            let reason = match err.kind() {
                                WhatsAppMarkerFailure::Refused => "trust boundary",
                                WhatsAppMarkerFailure::Failed => "not found",
                            };
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "kind": format!("{:?}", media.kind),
                                    "reason": reason,
                                    "error": err.to_string(),
                                })),
                                "whatsapp-web: dropping unresolved outbound attachment marker"
                            );
                            failed_marker_count += 1;
                            continue;
                        }
                    };
                    Self::send_media_marker(&client, &to, media, &target).await
                }
            };
            match result {
                Ok(()) => delivered_markers += 1,
                Err(err) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "kind": marker.kind_label(),
                                "error": err.to_string(),
                            })),
                        "whatsapp-web: marker delivery failed"
                    );
                    failed_marker_count += 1;
                }
            }
        }

        if let Some(note) = whatsapp_delivery_failure_note(failed_marker_count) {
            if text_content.is_empty() {
                text_content = note;
            } else {
                text_content.push_str("\n\n");
                text_content.push_str(&note);
            }
        }

        if !markers.is_empty() && text_content.is_empty() && delivered_markers > 0 {
            return Ok(());
        }

        // Send text message
        let outgoing = waproto::whatsapp::Message {
            conversation: Some(text_content),
            ..Default::default()
        };

        // Box::pin the large future (~34KB) so it doesn't inflate the
        // enclosing Send future's stack slot — clippy::large_futures.
        // whatsapp-rust 0.6: send_message returns `SendResult { message_id, to }`
        // instead of a bare `String` (oxidezap/whatsapp-rust
        let send_result = Box::pin(client.send_message(to, outgoing)).await?;
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "sent text to {} (id: {})",
                message.recipient, send_result.message_id
            )
        );
        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> Result<()> {
        // Store the sender channel for incoming messages
        *self.tx.lock() = Some(tx.clone());

        // Capture alias as an Arc so the long-running event closure (inside
        // the reconnect loop) can clone cheaply per spawned message without
        // borrowing `self` for its 'static lifetime.
        let alias = std::sync::Arc::new(self.alias.clone());

        use wacore::proto_helpers::MessageExt;
        use wacore::store::DevicePropsOverride;
        use wacore::types::events::Event;
        use wacore_binary::jid::JidExt as _;
        use whatsapp_rust::TokioRuntime;
        use whatsapp_rust::bot::Bot;
        use whatsapp_rust::pair_code::PairCodeOptions;
        use whatsapp_rust::store::{Device, DeviceStore};
        use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
        use whatsapp_rust_ureq_http_client::UreqHttpClient;

        let retry_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

        loop {
            let expanded_session_path = Self::expand_session_path(&self.session_path);

            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                &format!("channel starting (session: {})", expanded_session_path)
            );

            // Initialize storage backend
            let storage = RusqliteStore::new(&expanded_session_path)?;
            let backend = Arc::new(storage);

            // Check if we have a saved device to load
            let mut device = Device::new(backend.clone());
            if backend.exists().await? {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "found existing session, loading device"
                );
                if let Some(core_device) = backend.load().await? {
                    device.load_from_serializable(core_device);
                } else {
                    anyhow::bail!("Device exists but failed to load");
                }
                if let Some(ref pn) = device.pn
                    && let Some(digits) = Self::store_jid_digits(&self.bot_phone, pn.user())
                {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!("pre-resolved bot phone from saved session: +{}", digits)
                    );
                }
                if let Some(ref lid) = device.lid
                    && let Some(digits) = Self::store_jid_digits(&self.bot_lid, lid.user())
                {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!("pre-resolved bot LID from saved session: {}", digits)
                    );
                }
            } else {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "no existing session, new device will be created during pairing"
                );
            };

            // Create transport factory. WebSocket URL override comes from
            // `[whatsapp.ws_url]`; legacy `WHATSAPP_WS_URL` env var is gone.
            let mut transport_factory = TokioWebSocketTransportFactory::new();
            if let Some(ref ws_url) = self.ws_url {
                transport_factory = transport_factory.with_url(ws_url.clone());
            }

            // Create HTTP client for media operations
            let http_client = UreqHttpClient::new();

            // Channel to signal logout from the event handler back to the listen loop.
            let (logout_tx, mut logout_rx) = tokio::sync::broadcast::channel::<()>(1);

            // Tracks whether Event::LoggedOut actually fired (vs task crash).
            let session_revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // Build the bot
            let tx_clone = tx.clone();
            let peer_resolver = Arc::clone(&self.peer_resolver);
            let logout_tx_clone = logout_tx.clone();
            let retry_count_clone = retry_count.clone();
            let session_revoked_clone = session_revoked.clone();
            let transcription_config = self.transcription.clone();
            let transcription_mgr = self.transcription_manager.clone();
            let voice_chats = self.voice_chats.clone();
            let wa_mode = self.mode.clone();
            let wa_dm_policy = self.dm_policy.clone();
            let wa_group_policy = self.group_policy.clone();
            let wa_self_chat_mode = self.self_chat_mode;
            let mention_only = self.mention_only;
            let passive_group_context = self.passive_group_context;
            let bot_phone_clone = self.bot_phone.clone();
            let bot_lid_clone = self.bot_lid.clone();
            let wa_dm_mention_patterns = self.dm_mention_patterns.clone();
            let wa_group_mention_patterns = self.group_mention_patterns.clone();
            let allowed_groups_resolver = Arc::clone(&self.allowed_groups_resolver);
            let persist_clone = self.persist.clone();

            let mut builder = Bot::builder()
                .with_backend(backend)
                .with_transport_factory(transport_factory)
                .with_http_client(http_client)
                .with_runtime(TokioRuntime)
                .with_device_props(
                    DevicePropsOverride::new()
                        .with_os("ZeroClaw")
                        .with_platform_type(PlatformType::Desktop),
                )
                .on_event({
                    let alias = Arc::clone(&alias);
                    move |event, client| {
                    let tx_inner = tx_clone.clone();
                    let peer_resolver = Arc::clone(&peer_resolver);
                    let logout_tx = logout_tx_clone.clone();
                    let retry_count = retry_count_clone.clone();
                    let session_revoked = session_revoked_clone.clone();
                    let alias = Arc::clone(&alias);
                    let transcription_config = transcription_config.clone();
                    let transcription_mgr = transcription_mgr.clone();
                    let voice_chats = voice_chats.clone();
                    let wa_mode = wa_mode.clone();
                    let wa_dm_policy = wa_dm_policy.clone();
                    let wa_group_policy = wa_group_policy.clone();
                    let passive_group_context = passive_group_context;
                    let bot_phone_inner = bot_phone_clone.clone();
                    let bot_lid_inner = bot_lid_clone.clone();
                    let wa_dm_mention_patterns = wa_dm_mention_patterns.clone();
                    let wa_group_mention_patterns = wa_group_mention_patterns.clone();
                    let allowed_groups_resolver = Arc::clone(&allowed_groups_resolver);
                    let persist_inner = persist_clone.clone();
                    async move {
                        // whatsapp-rust 0.6: event handlers receive `Arc<Event>`
                        // per so we match against `&*event` to get a
                        // `&Event` reference and bind variant fields by ref.
                        match &*event {
                            Event::Message(msg, info) => {
                                let sender_jid = info.source.sender.clone();
                                let sender_alt = info.source.sender_alt.clone();
                                let sender = sender_jid.user().to_string();
                                let chat = info.source.chat.to_string();

                                let mapped_phone = if sender_jid.is_lid() {
                                    match client.get_lid_pn_entry(&sender_jid).await {
                                        Ok(Some(entry)) => Some(entry.phone_number),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                let sender_candidates = Self::sender_phone_candidates(
                                    &sender_jid,
                                    sender_alt.as_ref(),
                                    mapped_phone.as_deref(),
                                );

                                let allowed_peers = peer_resolver();
                                let normalized = sender_candidates
                                    .iter()
                                    .find(|candidate| {
                                        Self::is_number_allowed_for_list(&allowed_peers, candidate)
                                    })
                                    .cloned();

                                let is_group = info.source.is_group;
                                let reply_target = Self::compute_reply_target(&chat);

                                let allowed_groups = allowed_groups_resolver();
                                if is_group && !is_group_chat_allowed(&chat, &allowed_groups) {
                                    ::zeroclaw_log::record!(
                                        DEBUG,
                                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                            .with_attrs(::serde_json::json!({ "chat": chat })),
                                        "dropping group message: chat not in allowed_groups"
                                    );
                                    return;
                                }

                                // ── Personal-mode chat-type policy filtering ──
                                if wa_mode == zeroclaw_config::schema::WhatsAppWebMode::Personal {
                                    // Self-chat: the chat JID user part matches
                                    // the sender's user part (message to "Notes
                                    // to Self").
                                    let sender_user = sender_jid.user();
                                    let chat_user = chat
                                        .split_once('@')
                                        .map(|(u, _)| u)
                                        .unwrap_or(&chat);
                                    let is_self_chat = !is_group && sender_user == chat_user && info.source.is_from_me;

                                    if is_self_chat {
                                        if !wa_self_chat_mode {
                                            ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "ignoring self-chat message (self_chat_mode=false)");
                                            return;
                                        }
                                        // self_chat_mode=true: always process, skip further policy checks.
                                    } else if info.source.is_from_me
                                        && !fromme_outside_self_chat_is_operator_trigger(
                                            is_group,
                                            &wa_dm_mention_patterns,
                                            &wa_group_mention_patterns,
                                            msg.text_content().unwrap_or(""),
                                        )
                                    {
                                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"chat": chat, "sender": sender})), "ignoring fromMe message outside self-chat thread (chat=, sender=)");
                                        return;
                                    } else if is_group {
                                        match wa_group_policy {
                                            zeroclaw_config::schema::WhatsAppChatPolicy::Ignore => {
                                                ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "ignoring group message (group_policy=ignore)");
                                                return;
                                            }
                                            zeroclaw_config::schema::WhatsAppChatPolicy::All => {
                                                // allow unconditionally
                                            }
                                            zeroclaw_config::schema::WhatsAppChatPolicy::Allowlist => {
                                                if normalized.is_none() {
                                                    let lid_diag = Self::lid_rejection_diagnostic(
                                                        &sender_jid,
                                                        mapped_phone.as_deref(),
                                                    );
                                                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown), &format!("message from unrecognized sender not in allowed list (candidates_count={}){}", sender_candidates.len(), lid_diag));
                                                    return;
                                                }
                                            }
                                        }
                                    } else {
                                        // DM (non-self)
                                        match wa_dm_policy {
                                            zeroclaw_config::schema::WhatsAppChatPolicy::Ignore => {
                                                ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "ignoring DM (dm_policy=ignore)");
                                                return;
                                            }
                                            zeroclaw_config::schema::WhatsAppChatPolicy::All => {
                                                // allow unconditionally
                                            }
                                            zeroclaw_config::schema::WhatsAppChatPolicy::Allowlist => {
                                                if normalized.is_none() {
                                                    let lid_diag = Self::lid_rejection_diagnostic(
                                                        &sender_jid,
                                                        mapped_phone.as_deref(),
                                                    );
                                                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown), &format!("message from unrecognized sender not in allowed list (candidates_count={}){}", sender_candidates.len(), lid_diag));
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                }

                                let normalized = normalized.unwrap_or_else(|| sender.clone());
                                let conversation_scope =
                                    Self::group_context_scope(passive_group_context, is_group);
                                let mut passive_context = false;
                                let text_content = msg.text_content().unwrap_or("").trim().to_string();
                                let mut content = Self::media_fallback_content(text_content, msg);

                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("WhatsApp Web message received (sender_len={}, chat_len={}, content_len={})", sender.len(), chat.len(), content.len()));
                                ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("WhatsApp Web message content: {}", content));

                                // mention_only: group messages without a bot mention can become
                                // passive context when explicitly enabled; otherwise they keep
                                // the existing drop behavior. This runs before STT/media
                                // downloads so passive messages have no provider/tool side effects.
                                if mention_only && is_group {
                                    let bot_phone = bot_phone_inner.lock();
                                    let bot_lid = bot_lid_inner.lock();
                                    if bot_phone.is_some() || bot_lid.is_some() {
                                        let bp = bot_phone.as_deref().unwrap_or("");
                                        let bl = bot_lid.as_deref();
                                        let addressed =
                                            Self::is_message_addressed_to_bot(msg, &content, bp, bl);
                                        if Self::should_record_passive_group_context(
                                            passive_group_context,
                                            is_group,
                                            addressed,
                                        ) {
                                            passive_context = true;
                                        } else if !addressed {
                                            ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "ignoring group message not addressed to bot");
                                            return;
                                        }
                                    } else {
                                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "mention_only active but bot identity unknown, skipping group msg");
                                        return;
                                    }
                                }

                                let passive_from_mention_gating_possible =
                                    Self::should_record_passive_group_context(
                                        passive_group_context,
                                        is_group,
                                        false,
                                    );
                                if !passive_context && passive_from_mention_gating_possible {
                                    match super::whatsapp::WhatsAppChannel::apply_mention_gating(
                                        &wa_dm_mention_patterns,
                                        &wa_group_mention_patterns,
                                        &content,
                                        is_group,
                                    ) {
                                        Some(c) => content = c,
                                        None => {
                                            passive_context = true;
                                        }
                                    }
                                }

                                if passive_context {
                                    if content.is_empty() {
                                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("ignoring empty passive group context from {}", normalized));
                                        return;
                                    }
                                    Self::send_inbound_channel_message(
                                        &tx_inner,
                                        alias.as_ref(),
                                        &normalized,
                                        reply_target,
                                        content,
                                        Vec::new(),
                                        true,
                                        conversation_scope,
                                    )
                                    .await;
                                    return;
                                }

                                // Attempt voice note transcription (ptt = push-to-talk = voice note).
                                // When `transcribe_non_ptt_audio` is enabled in the transcription
                                // config, also transcribe forwarded / regular audio messages.
                                let voice_text = if let Some(ref audio) = msg.audio_message {
                                    let is_ptt = audio.ptt == Some(true);
                                    let non_ptt_enabled = transcription_config
                                        .as_ref()
                                        .is_some_and(|c| c.transcribe_non_ptt_audio);
                                    if is_ptt || non_ptt_enabled {
                                        Self::try_transcribe_voice_note(
                                            &client,
                                            audio,
                                            transcription_config.as_ref(),
                                            transcription_mgr.as_deref(),
                                        )
                                        .await
                                    } else {
                                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("ignoring non-PTT audio message from {}", normalized));
                                        None
                                    }
                                } else {
                                    None
                                };

                                // Use transcribed voice text, or fall back to text content.
                                // Track whether this chat used a voice note so we reply in kind.
                                if let Some(ref vt) = voice_text {
                                    if let Ok(mut vs) = voice_chats.lock() {
                                        vs.insert(reply_target.clone());
                                    }
                                    content = format!("[Voice] {vt}");
                                } else if let Ok(mut vs) = voice_chats.lock() {
                                    vs.remove(&reply_target);
                                }
                                content = Self::media_fallback_content(content, msg);

                                if content.is_empty() {
                                    ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("ignoring empty or non-text message from {}", normalized));
                                    return;
                                }

                                if !passive_from_mention_gating_possible {
                                    content = match super::whatsapp::WhatsAppChannel::apply_mention_gating(
                                        &wa_dm_mention_patterns,
                                        &wa_group_mention_patterns,
                                        &content,
                                        is_group,
                                    ) {
                                        Some(c) => c,
                                        None => {
                                            ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"normalized": normalized})), "message from did not match mention patterns, dropping");
                                            return;
                                        }
                                    };
                                }

                                let mut attachments = Vec::new();
                                Self::collect_media_attachments(
                                    &client,
                                    msg,
                                    "",
                                    false,
                                    &mut attachments,
                                )
                                .await;
                                if let Some(quoted) = Self::extract_quoted_message(msg) {
                                    Self::collect_media_attachments(
                                        &client,
                                        quoted,
                                        "quoted-",
                                        true,
                                        &mut attachments,
                                    )
                                    .await;
                                }

                                Self::send_inbound_channel_message(
                                    &tx_inner,
                                    alias.as_ref(),
                                    &normalized,
                                    reply_target,
                                    content,
                                    attachments,
                                    false,
                                    conversation_scope,
                                )
                                .await;
                            }
                            Event::Connected(_) => {
                                crate::login_events::LoginEvent::Connected.emit(
                                    "whatsapp",
                                    alias.as_ref(),
                                    "WhatsApp Web connected successfully",
                                );
                                WhatsAppWebChannel::reset_retry(&retry_count);
                                let device = client
                                    .persistence_manager()
                                    .get_device_snapshot()
                                    .await;
                                // Resolve bot identity from the device store
                                if mention_only {
                                    if let Some(ref pn) = device.pn
                                        && let Some(digits) =
                                            Self::store_jid_digits(&bot_phone_inner, pn.user())
                                    {
                                        ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("resolved bot identity from device: +{}", digits));
                                    }
                                    if let Some(ref lid) = device.lid
                                        && let Some(digits) =
                                            Self::store_jid_digits(&bot_lid_inner, lid.user())
                                    {
                                        ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), &format!("resolved bot LID from device: {}", digits));
                                    }
                                }
                                // Persist the linked account as an authorized
                                // peer in canonical peer_groups (same shape
                                // WeChat pairing writes). Idempotent, so the
                                // reconnect-after-resume case is a no-op.
                                if let Some(ref pn) = device.pn {
                                    let digits = Self::jid_digits(pn.user());
                                    if !digits.is_empty()
                                        && let Err(e) =
                                            crate::identity_persist::persist_external_peer(
                                                persist_inner.as_ref(),
                                                "whatsapp",
                                                alias.as_ref(),
                                                &format!("+{digits}"),
                                            )
                                            .await
                                    {
                                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown), &format!("failed to persist linked WhatsApp identity: {e}"));
                                    }
                                }
                            }
                            Event::LoggedOut(_) => {
                                session_revoked.store(true, std::sync::atomic::Ordering::Relaxed);
                                crate::login_events::LoginEvent::LoggedOut.emit(
                                    "whatsapp",
                                    alias.as_ref(),
                                    "WhatsApp Web was logged out — will clear session and reconnect",
                                );
                                let _ = logout_tx.send(());
                            }
                            Event::StreamError(stream_error) => {
                                ::zeroclaw_log::record!(ERROR, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail).with_outcome(::zeroclaw_log::EventOutcome::Failure), &format!("stream error: {:?}", stream_error));
                            }
                            Event::PairingCode { code, .. } => {
                                crate::login_events::LoginEvent::PairCode { code: code.as_str() }
                                    .emit(
                                    "whatsapp",
                                    alias.as_ref(),
                                    "WhatsApp Web pair code received — enter it in WhatsApp > Linked Devices",
                                );
                                eprintln!();
                                eprintln!("pair code: {code}");
                                eprintln!();
                            }
                            Event::PairingQrCode { code, .. } => {
                                crate::login_events::LoginEvent::Qr {
                                    payload: code.as_str(),
                                    image_url: None,
                                    attempt: None,
                                    max_attempts: None,
                                }
                                .emit(
                                    "whatsapp",
                                    alias.as_ref(),
                                    "WhatsApp Web QR code received (scan with WhatsApp > Linked Devices)",
                                );
                                match Self::render_pairing_qr(code) {
                                    Ok(rendered) => {
                                        eprintln!();
                                        eprintln!(
                                            "WhatsApp Web QR code (scan in WhatsApp > Linked Devices):"
                                        );
                                        eprintln!("{rendered}");
                                        eprintln!();
                                    }
                                    Err(err) => {
                                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown), &format!("failed to render pairing QR in terminal: {}", err));
                                        eprintln!();
                                        eprintln!("WhatsApp Web QR payload: {code}");
                                        eprintln!();
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }});

            // Configure pair-code flow when a phone number is provided.
            if let Some(ref phone) = self.pair_phone {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "pair-code flow enabled for configured phone number"
                );
                builder = builder.with_pair_code(PairCodeOptions {
                    phone_number: phone.clone(),
                    custom_code: self.pair_code.clone(),
                    ..Default::default()
                });
            } else if self.pair_code.is_some() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "pair_code is set but pair_phone is missing; pair code config is ignored"
                );
            }

            let mut bot = builder.build().await?;
            *self.client.lock() = Some(bot.client());

            // Run the bot
            let bot_handle = bot.run().await?;

            // Store the bot handle for later shutdown
            *self.bot_handle.lock() = Some(bot_handle);

            // Drop the outer sender so logout_rx.recv() returns Err when the
            // bot task ends without emitting LoggedOut (e.g. crash/panic).
            drop(logout_tx);

            // Wait for a logout signal or process shutdown.
            let should_reconnect = select! {
                res = logout_rx.recv() => {
                    // Both Ok(()) and Err (sender dropped) mean the session ended.
                    let _ = res;
                    true
                }
                _ = tokio::signal::ctrl_c() => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "channel received Ctrl+C");
                    false
                }
            };

            *self.client.lock() = None;
            let handle = self.bot_handle.lock().take();
            if let Some(handle) = handle {
                handle.abort();
                // Await the aborted task so background I/O finishes before
                // we delete session files.
                let _ = handle.await;
            }

            // Drop bot/device so the SQLite connection is closed
            // before we remove session files (releases WAL/SHM locks).
            // `backend` was moved into the builder, so dropping `bot`
            // releases the last Arc reference to the storage backend.
            drop(bot);
            drop(device);

            if should_reconnect {
                let (attempts, exceeded) = Self::record_retry(&retry_count);
                if exceeded {
                    anyhow::bail!(
                        "exceeded {} reconnect attempts, giving up",
                        Self::MAX_RETRIES
                    );
                }

                // Only purge session files when LoggedOut was explicitly observed.
                // A transient task crash (Err from recv) should not wipe a valid session.
                if Self::should_purge_session(&session_revoked) {
                    for path in Self::session_file_paths(&expanded_session_path) {
                        match tokio::fs::remove_file(&path).await {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                                &format!("failed to remove session file {}: {e}", path)
                            ),
                        }
                    }
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        "session files removed, restarting for QR pairing"
                    );
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "bot stopped without LoggedOut; reconnecting with existing session"
                    );
                }

                let delay = Self::compute_retry_delay(attempts);
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!(
                        "reconnecting in {}s (attempt {}/{})",
                        delay,
                        attempts,
                        Self::MAX_RETRIES
                    )
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                continue;
            }

            break;
        }

        Ok(())
    }

    async fn health_check(&self) -> bool {
        let bot_handle_guard = self.bot_handle.lock();
        bot_handle_guard.is_some()
    }

    async fn start_typing(&self, recipient: &str) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        if !Self::is_jid(recipient) {
            let normalized = self.normalize_phone(recipient);
            if !self.is_number_allowed(&normalized) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!("typing target {} not in allowed list", recipient)
                );
                return Ok(());
            }
        }

        let deliverable_recipient = Self::resolve_outbound_recipient(recipient);
        let to = self.recipient_to_jid(&deliverable_recipient)?;
        client.chatstate().send_composing(&to).await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Failed to send typing state (composing)"
            );
            anyhow::Error::msg(format!("Failed to send typing state (composing): {e}"))
        })?;

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("start typing for {}", recipient)
        );
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        if !Self::is_jid(recipient) {
            let normalized = self.normalize_phone(recipient);
            if !self.is_number_allowed(&normalized) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!("typing target {} not in allowed list", recipient)
                );
                return Ok(());
            }
        }

        let deliverable_recipient = Self::resolve_outbound_recipient(recipient);
        let to = self.recipient_to_jid(&deliverable_recipient)?;
        client.chatstate().send_paused(&to).await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Failed to send typing state (paused)"
            );
            anyhow::Error::msg(format!("Failed to send typing state (paused): {e}"))
        })?;

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!("stop typing for {}", recipient)
        );
        Ok(())
    }
}

// Stub implementation when feature is not enabled
#[cfg(not(feature = "whatsapp-web"))]
pub struct WhatsAppWebChannel {
    _private: (),
}

#[cfg(not(feature = "whatsapp-web"))]
impl WhatsAppWebChannel {
    pub fn new(
        _session_path: String,
        _pair_phone: Option<String>,
        _pair_code: Option<String>,
        _ws_url: Option<String>,
        _alias: impl Into<String>,
        _peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        _mention_only: bool,
        _mode: zeroclaw_config::schema::WhatsAppWebMode,
        _dm_policy: zeroclaw_config::schema::WhatsAppChatPolicy,
        _group_policy: zeroclaw_config::schema::WhatsAppChatPolicy,
        _self_chat_mode: bool,
    ) -> Self {
        Self { _private: () }
    }

    pub fn with_transcription(self, _config: zeroclaw_config::schema::TranscriptionConfig) -> Self {
        self
    }

    pub fn with_tts(self, _config: zeroclaw_config::schema::TtsConfig) -> Self {
        self
    }
}

#[cfg(not(feature = "whatsapp-web"))]
impl ::zeroclaw_api::attribution::Attributable for WhatsAppWebChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(
            ::zeroclaw_api::attribution::ChannelKind::WhatsappWeb,
        )
    }
    fn alias(&self) -> &str {
        "whatsapp"
    }
}

#[cfg(not(feature = "whatsapp-web"))]
#[async_trait]
impl Channel for WhatsAppWebChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    async fn send(&self, _message: &SendMessage) -> Result<()> {
        anyhow::bail!(i18n::get_required_cli_string(
            "channel-whatsapp-web-feature-missing-error"
        ));
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> Result<()> {
        anyhow::bail!(i18n::get_required_cli_string(
            "channel-whatsapp-web-feature-missing-error"
        ));
    }

    async fn health_check(&self) -> bool {
        false
    }

    async fn start_typing(&self, _recipient: &str) -> Result<()> {
        anyhow::bail!(i18n::get_required_cli_string(
            "channel-whatsapp-web-feature-missing-error"
        ));
    }

    async fn stop_typing(&self, _recipient: &str) -> Result<()> {
        anyhow::bail!(i18n::get_required_cli_string(
            "channel-whatsapp-web-feature-missing-error"
        ));
    }
}

#[cfg(test)]
mod tests;
