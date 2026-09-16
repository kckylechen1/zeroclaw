//! Matrix channel using matrix-rust-sdk 0.16.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock, mpsc, oneshot};

use matrix_sdk::{
    Client,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId,
        api::client::{
            membership::invite_user::v3::{
                InvitationRecipient, InviteUserId, Request as InviteUserRequest,
            },
            room::{Visibility as MatrixVisibility, create_room::v3::Request as CreateRoomRequest},
        },
        events::{InitialStateEvent, room::encryption::RoomEncryptionEventContent},
    },
};

use zeroclaw_api::channel::{
    Channel, ChannelApprovalRequest, ChannelApprovalResponse, ChannelMessage, RoomCreationOptions,
    RoomVisibility, SendMessage,
};
use zeroclaw_config::schema::{MatrixConfig, StreamMode, TranscriptionConfig};

// ─── markers ───────────────────────────────────────────────────────────────
mod markers {
    //! Parse `[image:url]`, `[audio:url]`, `[video:url]`, `[file:url]`, `[voice:url]`
    //! markers from outbound text. Strips them from the body and returns the kinds
    //! + targets so the caller can upload the corresponding media.

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum MarkerKind {
        Image,
        Audio,
        Video,
        File,
        Voice,
    }

    impl MarkerKind {
        fn from_keyword(kw: &str) -> Option<Self> {
            match kw.to_ascii_lowercase().as_str() {
                "image" | "img" | "photo" => Some(Self::Image),
                "audio" => Some(Self::Audio),
                "video" => Some(Self::Video),
                "file" | "document" | "doc" => Some(Self::File),
                "voice" => Some(Self::Voice),
                _ => None,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Marker {
        pub kind: MarkerKind,
        pub target: String,
    }

    /// Scan `text` for marker substrings. Returns the cleaned text and any markers.
    /// Malformed/unknown markers are left in the text untouched.
    pub(super) fn parse(text: &str) -> (String, Vec<Marker>) {
        let mut out = String::with_capacity(text.len());
        let mut markers = Vec::new();
        let mut chars = text.char_indices().peekable();

        while let Some((start, ch)) = chars.next() {
            if ch != '[' {
                out.push(ch);
                continue;
            }

            let rest = &text[start + 1..];
            let Some(close_rel) = rest.find(']') else {
                out.push(ch);
                continue;
            };
            if rest[..close_rel].contains('\n') {
                out.push(ch);
                continue;
            }
            let inner = &rest[..close_rel];
            let Some(colon) = inner.find(':') else {
                out.push(ch);
                continue;
            };
            let kw = &inner[..colon];
            let target = inner[colon + 1..].trim();

            let Some(kind) = MarkerKind::from_keyword(kw) else {
                out.push(ch);
                continue;
            };
            if target.is_empty() {
                out.push(ch);
                continue;
            }

            markers.push(Marker {
                kind,
                target: target.to_string(),
            });
            let consume_until = start + 1 + close_rel + 1;
            while let Some(&(idx, _)) = chars.peek() {
                if idx >= consume_until {
                    break;
                }
                chars.next();
            }
        }

        // Tidy whitespace left behind by stripped markers.
        let cleaned = out
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        (cleaned.trim().to_string(), markers)
    }
}

// ─── mention ───────────────────────────────────────────────────────────────
mod mention {
    use matrix_sdk::ruma::UserId;

    pub(super) fn is_mentioned(
        bot_user_id: &UserId,
        bot_display_name: Option<&str>,
        m_mentions_user_ids: Option<&[String]>,
        body: &str,
    ) -> bool {
        if let Some(ids) = m_mentions_user_ids {
            for id in ids {
                if id == bot_user_id.as_str() {
                    return true;
                }
            }
            // Honour the explicit list when set — older clients without
            // `m.mentions` still hit the body-scan fallback below.
            if !ids.is_empty() {
                return false;
            }
        }

        let body_lc = body.to_ascii_lowercase();
        if body_lc.contains(&bot_user_id.as_str().to_ascii_lowercase()) {
            return true;
        }
        let localpart = bot_user_id.localpart().to_ascii_lowercase();
        if body_lc.contains(&format!("@{localpart}")) {
            return true;
        }
        if let Some(name) = bot_display_name
            && !name.is_empty()
        {
            let n = name.to_ascii_lowercase();
            if body_lc.contains(&n) {
                return true;
            }
        }
        false
    }
}

// ─── allowlist ─────────────────────────────────────────────────────────────
mod allowlist {
    pub(super) fn user_allowed(allowed_users: &[String], sender: &str) -> bool {
        crate::allowlist::is_user_allowed(
            allowed_users,
            sender,
            crate::allowlist::Match::CaseInsensitive,
        )
    }

    pub(super) fn room_allowed_static(allowed_rooms: &[String], room_id: &str) -> bool {
        if allowed_rooms.is_empty() {
            return true;
        }
        allowed_rooms
            .iter()
            .any(|r| r == room_id || r.eq_ignore_ascii_case(room_id))
    }
}

// ─── approval ──────────────────────────────────────────────────────────────
mod approval {
    use rand::{Rng, RngExt};
    use zeroclaw_api::channel::ChannelApprovalResponse;

    pub(super) const TOKEN_LEN: usize = 8;
    const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

    pub(super) fn generate_token<R: Rng>(rng: &mut R) -> String {
        (0..TOKEN_LEN)
            .map(|_| TOKEN_ALPHABET[rng.random_range(0..TOKEN_ALPHABET.len())] as char)
            .collect()
    }

    pub(super) fn generate_token_default() -> String {
        let mut rng = rand::rng();
        generate_token(&mut rng)
    }

    /// Try to parse an approval reply. Returns `Some((token, response))` if the
    /// body matches `<TOKEN> (approve|deny|always|yes|no)` (case-insensitive).
    pub(super) fn parse_reply(body: &str) -> Option<(String, ChannelApprovalResponse)> {
        let trimmed = body.trim();
        let mut parts = trimmed.split_whitespace();
        let token = parts.next()?;
        if token.len() != TOKEN_LEN {
            return None;
        }
        if !token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let verb = parts.next()?.to_ascii_lowercase();
        if parts.next().is_some() {
            return None;
        }
        let response = match verb.as_str() {
            "approve" | "yes" | "y" => ChannelApprovalResponse::Approve,
            "deny" | "no" | "n" => ChannelApprovalResponse::Deny,
            "always" => ChannelApprovalResponse::AlwaysApprove,
            _ => return None,
        };
        Some((token.to_uppercase(), response))
    }
}

// ─── room management ──────────────────────────────────────────────────────
mod room_management {
    use super::*;

    pub(super) fn build_create_room_request(
        options: &RoomCreationOptions,
    ) -> Result<CreateRoomRequest> {
        let mut request = CreateRoomRequest::new();
        request.name = options.name.clone();
        request.topic = options.topic.clone();
        request.invite = options
            .invites
            .iter()
            .map(|user_id| {
                user_id
                    .parse::<OwnedUserId>()
                    .with_context(|| format!("matrix: invalid invite user id '{user_id}'"))
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(visibility) = options.visibility {
            request.visibility = match visibility {
                RoomVisibility::Private => MatrixVisibility::Private,
                RoomVisibility::Public => MatrixVisibility::Public,
            };
        }
        if options.encryption.unwrap_or(false) {
            request.initial_state.push(
                InitialStateEvent::with_empty_state_key(
                    RoomEncryptionEventContent::with_recommended_defaults(),
                )
                .to_raw_any(),
            );
        }
        Ok(request)
    }

    pub(super) fn build_invite_user_request(
        room_id: &str,
        user_id: &str,
    ) -> Result<InviteUserRequest> {
        let room_id = room_id
            .parse::<OwnedRoomId>()
            .with_context(|| format!("matrix: invalid room id '{room_id}'"))?;
        let user_id = user_id
            .parse::<OwnedUserId>()
            .with_context(|| format!("matrix: invalid user id '{user_id}'"))?;
        Ok(InviteUserRequest::new(
            room_id,
            InvitationRecipient::from(InviteUserId::new(user_id)),
        ))
    }
}

// ─── context (thread-root preamble) ────────────────────────────────────────
mod context {
    //! Inject the thread root as a `[Thread root from @x]: ...` preamble on the
    //! first inbound message we see in each thread. After a restart we re-inject
    //! exactly once per active thread (in-memory tracking only).

    use std::{collections::HashSet, sync::Arc};

    use matrix_sdk::ruma::{OwnedEventId, events::room::message::MessageType};
    use tokio::sync::RwLock;

    pub(super) fn format_preamble(sender: &str, body: &str) -> String {
        let body = body.trim();
        if body.is_empty() {
            format!("[Thread root from {sender}]\n\n")
        } else {
            format!("[Thread root from {sender}]: {body}\n\n")
        }
    }

    /// Returns `true` iff this thread had not been seen before — caller should
    /// fetch the root and inject the preamble. Also marks the thread seen.
    pub(super) async fn claim_first_visit(
        threads_seen: &Arc<RwLock<HashSet<OwnedEventId>>>,
        thread_id: &OwnedEventId,
    ) -> bool {
        let mut guard = threads_seen.write().await;
        guard.insert(thread_id.clone())
    }

    /// Pre-mark a thread — used when the bot starts the thread itself, so the
    /// next inbound thread message doesn't get a preamble pointing at the bot.
    pub(super) async fn mark_seen(
        threads_seen: &Arc<RwLock<HashSet<OwnedEventId>>>,
        thread_id: OwnedEventId,
    ) {
        threads_seen.write().await.insert(thread_id);
    }

    pub(super) fn body_for(msg: &MessageType) -> String {
        match msg {
            MessageType::Text(t) => t.body.clone(),
            MessageType::Notice(n) => n.body.clone(),
            MessageType::Emote(e) => e.body.clone(),
            MessageType::Image(_) => "[image]".to_string(),
            MessageType::File(_) => "[file]".to_string(),
            MessageType::Audio(_) => "[audio]".to_string(),
            MessageType::Video(_) => "[video]".to_string(),
            MessageType::Location(_) => "[location]".to_string(),
            other => other.body().to_string(),
        }
    }
}

// ─── streaming ─────────────────────────────────────────────────────────────
mod streaming {
    use std::{
        collections::HashMap,
        time::{Duration, Instant},
    };

    use anyhow::{Result, bail};
    use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};

    use super::markers;

    const MULTI_MESSAGE_SYNTHETIC_PREFIX: &str = "multi_message_synthetic:";

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(super) struct DraftKey {
        room_id: OwnedRoomId,
        draft_id: String,
    }

    pub(super) fn draft_key(room_id: OwnedRoomId, draft_id: &str) -> Result<DraftKey> {
        let draft_id = draft_id.trim();
        if draft_id.is_empty() {
            bail!("matrix: draft message id is empty");
        }
        Ok(DraftKey {
            room_id,
            draft_id: draft_id.to_string(),
        })
    }

    pub(super) fn new_multi_message_draft_id() -> String {
        format!(
            "{MULTI_MESSAGE_SYNTHETIC_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        )
    }

    #[derive(Debug, Clone)]
    pub(super) struct PartialDraft {
        pub event_id: OwnedEventId,
        pub thread_anchor: Option<OwnedEventId>,
        pub last_text: String,
        pub last_edit: Instant,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum PartialFinalizeAction {
        EditDraft,
        RedactDraft,
        EmptyError,
    }

    #[derive(Debug, Clone)]
    pub(super) struct MultiDraft {
        pub thread_anchor: Option<OwnedEventId>,
        pub sent_so_far: usize,
    }

    #[derive(Default, Debug)]
    pub(super) struct State {
        pub partial: HashMap<DraftKey, PartialDraft>,
        pub multi: HashMap<DraftKey, MultiDraft>,
    }

    pub(super) fn partial_for_update<'a>(
        state: &'a mut State,
        key: &DraftKey,
    ) -> Option<&'a mut PartialDraft> {
        state.partial.get_mut(key)
    }

    pub(super) fn take_partial(state: &mut State, key: &DraftKey) -> Option<PartialDraft> {
        state.partial.remove(key)
    }

    pub(super) fn multi_for_update<'a>(
        state: &'a mut State,
        key: &DraftKey,
    ) -> Option<&'a mut MultiDraft> {
        state.multi.get_mut(key)
    }

    pub(super) fn take_multi(state: &mut State, key: &DraftKey) -> Option<MultiDraft> {
        state.multi.remove(key)
    }

    pub(super) fn partial_should_edit(
        existing: &PartialDraft,
        new_text: &str,
        now: Instant,
        min_interval: Duration,
    ) -> bool {
        if existing.last_text == new_text {
            return false;
        }
        now.saturating_duration_since(existing.last_edit) >= min_interval
    }

    pub(super) fn partial_visible_text(text: &str) -> Option<String> {
        let (cleaned, _) = markers::parse(text);
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        }
    }

    pub(super) fn decide_partial_finalize_action(
        text_is_empty_after_delivery: bool,
        any_attachment_landed: bool,
    ) -> PartialFinalizeAction {
        match (text_is_empty_after_delivery, any_attachment_landed) {
            (false, _) => PartialFinalizeAction::EditDraft,
            (true, true) => PartialFinalizeAction::RedactDraft,
            (true, false) => PartialFinalizeAction::EmptyError,
        }
    }

    /// Find the next paragraph break (`\n\n`) in `new_text`, ignoring any
    /// breaks that fall inside an open ```fenced``` code block. Returns the
    /// byte offset of the first `\n` of the break, or `None` if no break is
    /// found yet (caller should buffer and retry on the next update).
    pub(super) fn next_paragraph_break(new_text: &str) -> Option<usize> {
        let bytes = new_text.as_bytes();
        let mut in_fence = false;
        let mut i = 0;
        while i < bytes.len() {
            // Detect opening or closing ```code fence``` at line start.
            if bytes[i] == b'`'
                && i + 2 < bytes.len()
                && bytes[i + 1] == b'`'
                && bytes[i + 2] == b'`'
                && (i == 0 || bytes[i - 1] == b'\n')
            {
                in_fence = !in_fence;
                i += 3;
                continue;
            }
            if !in_fence && bytes[i] == b'\n' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

// ─── session ───────────────────────────────────────────────────────────────
mod session {
    //! Persist the Matrix login session next to the SDK SQLite crypto store so
    //! `restore_session()` can reattach without re-running the login flow.

    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    pub(super) const SESSION_FILE: &str = "session.json";

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub(super) struct SessionBlob {
        pub user_id: String,
        pub device_id: String,
        pub access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub refresh_token: Option<String>,
    }

    pub(super) fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(SESSION_FILE)
    }

    pub(super) fn load(state_dir: &Path) -> anyhow::Result<Option<SessionBlob>> {
        let p = path(state_dir);
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p).map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": p.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "matrix: failed to read session blob"
            );
            anyhow::Error::msg(format!("read matrix session blob {}: {e}", p.display()))
        })?;
        match serde_json::from_slice::<SessionBlob>(&bytes) {
            Ok(blob) => Ok(Some(blob)),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    &format!(
                        "matrix: session blob {} is corrupt JSON ({e}); treating as missing so auto-recovery can re-login",
                        p.display()
                    )
                );
                Ok(None)
            }
        }
    }

    pub(super) fn save(state_dir: &Path, blob: &SessionBlob) -> anyhow::Result<()> {
        std::fs::create_dir_all(state_dir).map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": state_dir.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "matrix: failed to create state dir"
            );
            anyhow::Error::msg(format!(
                "create matrix state dir {}: {e}",
                state_dir.display()
            ))
        })?;
        let p = path(state_dir);
        let json = serde_json::to_vec_pretty(blob)?;
        write_with_owner_only(&p, &json).map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": p.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "matrix: failed to write session blob"
            );
            anyhow::Error::msg(format!("write matrix session blob {}: {e}", p.display()))
        })?;
        Ok(())
    }

    /// Write the session blob with `0o600` permissions on Unix so the
    /// access token isn't world-readable under a permissive umask.
    /// Windows falls back to default ACLs (the std-lib write).
    #[cfg(unix)]
    fn write_with_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)
    }

    #[cfg(not(unix))]
    fn write_with_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
        std::fs::write(path, contents)
    }
}

// ─── client ────────────────────────────────────────────────────────────────
mod client {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use anyhow::{Context as _, Result, bail};
    use matrix_sdk::{
        Client, SessionMeta, SessionTokens,
        authentication::matrix::MatrixSession,
        config::RequestConfig,
        ruma::{OwnedRoomId, RoomAliasId},
    };
    use serde::Deserialize;
    use tokio::sync::RwLock;

    use super::session;
    use zeroclaw_config::schema::MatrixConfig;

    const WHOAMI_ENDPOINT: &str = "_matrix/client/v3/account/whoami";

    pub(super) const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
    const WHOAMI_TIMEOUT: Duration = Duration::from_secs(30);
    const WHOAMI_ERROR_BODY_PREVIEW_BYTES: usize = 4096;
    const WHOAMI_ERROR_BODY_DISPLAY_CHARS: usize = 256;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct AccessTokenIdentity {
        pub user_id: String,
        pub device_id: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct WhoamiResponse {
        user_id: String,
        #[serde(default)]
        device_id: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct MatrixErrorResponse {
        #[serde(default)]
        errcode: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }

    pub(super) fn store_dir(state_dir: &Path) -> PathBuf {
        state_dir.join("store")
    }

    pub(super) async fn build(config: &MatrixConfig, state_dir: &Path) -> Result<Client> {
        build_attempt(config, state_dir, 0).await
    }

    fn wipe_state(state_dir: &Path) -> Result<()> {
        let session = session::path(state_dir);
        if session.exists()
            && let Err(e) = std::fs::remove_file(&session)
        {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": session.display().to_string(),
                        "phase": "corruption_recovery",
                        "error": format!("{}", e),
                    })),
                "matrix: failed to remove session blob during corruption recovery"
            );
            return Err(anyhow::Error::msg(format!(
                "matrix: failed to remove {} during corruption recovery: {e}. Fix permissions or wipe the directory manually.",
                session.display()
            )));
        }
        let store = store_dir(state_dir);
        if store.exists()
            && let Err(e) = std::fs::remove_dir_all(&store)
        {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": store.display().to_string(),
                        "phase": "corruption_recovery",
                        "error": format!("{}", e),
                    })),
                "matrix: failed to remove store dir during corruption recovery"
            );
            return Err(anyhow::Error::msg(format!(
                "matrix: failed to remove {} during corruption recovery: {e}. Fix permissions or wipe the directory manually.",
                store.display()
            )));
        }
        Ok(())
    }

    pub(super) fn store_has_orphan_data(state_dir: &Path) -> bool {
        let store = store_dir(state_dir);
        let Ok(mut entries) = std::fs::read_dir(&store) else {
            return false;
        };
        entries.any(|e| e.is_ok())
    }

    pub(super) fn can_password_relogin(config: &MatrixConfig) -> bool {
        let has_password = config
            .password
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let has_user_id = config
            .user_id
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        has_password && has_user_id
    }

    pub(super) fn saved_session_is_foreign(
        config: &MatrixConfig,
        blob: &session::SessionBlob,
    ) -> bool {
        let Some(want) = config.user_id.as_deref().filter(|s| !s.is_empty()) else {
            return false;
        };
        if !want.contains(':') {
            return false;
        }
        want != blob.user_id.as_str()
    }

    async fn build_attempt(
        config: &MatrixConfig,
        state_dir: &Path,
        recovery_attempts: u32,
    ) -> Result<Client> {
        // Hard recursion bound: at most one auto-wipe + relogin cycle per call.
        if recovery_attempts > 1 {
            bail!(
                "matrix: corruption recovery looped — aborting to avoid an infinite restart cycle. \
                 Wipe ~/.zeroclaw/state/matrix/ manually and restart."
            );
        }

        let saved = session::load(state_dir)?;

        // A saved session that belongs to a different account would run this
        // channel block as the wrong Matrix identity. Wipe and re-login fresh
        // under the configured account instead of impersonating.
        if let Some(blob) = saved.as_ref()
            && saved_session_is_foreign(config, blob)
        {
            return recover_or_bail(
                config,
                state_dir,
                recovery_attempts,
                &format!(
                    "saved session user_id ({}) does not match configured channels.matrix user_id ({}); store belongs to a different account.",
                    blob.user_id,
                    config.user_id.as_deref().unwrap_or_default()
                ),
            )
            .await;
        }

        if let (Some(blob), Some(want)) = (
            saved.as_ref(),
            config.device_id.as_deref().filter(|s| !s.is_empty()),
        ) && want != blob.device_id
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "matrix: configured channels.matrix.device-id ({want}) differs from the saved session ({}). \
                 Honoring the saved device_id (canonical, assigned by the homeserver). \
                 Update channels.matrix.device-id to match (or clear it) to silence this warning, \
                 or wipe {} entirely to register a different device.",
                    blob.device_id,
                    state_dir.display()
                )
            );
        }

        if saved.is_none() && store_has_orphan_data(state_dir) {
            return recover_or_bail(
                config,
                state_dir,
                recovery_attempts,
                "found crypto store data without a saved session.json — orphan state from a prior install or interrupted run.",
            )
            .await;
        }

        let store = store_dir(state_dir);
        std::fs::create_dir_all(&store)
            .with_context(|| format!("create matrix store dir {}", store.display()))?;

        let client = Client::builder()
            .homeserver_url(&config.homeserver)
            .sqlite_store(&store, None)
            // Widen the per-request timeout past the sync long-poll window so
            // an idle `/sync` never trips the SDK's default 30s request
            // deadline before the homeserver's own long-poll returns.
            .request_config(RequestConfig::new().timeout(CLIENT_REQUEST_TIMEOUT))
            .build()
            .await
            .context("build matrix client")?;

        // Step 1: restore an existing session, or fresh-login.
        if let Some(blob) = saved {
            let saved_device_id = blob.device_id.clone();
            let session = MatrixSession {
                meta: SessionMeta {
                    user_id: blob.user_id.parse().context("parse stored user_id")?,
                    device_id: blob.device_id.into(),
                },
                tokens: SessionTokens {
                    access_token: blob.access_token,
                    refresh_token: blob.refresh_token,
                },
            };
            match client
                .matrix_auth()
                .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
                .await
            {
                Ok(()) => ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "matrix: restored session from session.json"
                ),
                Err(e) => {
                    // restore_session failed despite a matching device_id —
                    // the access token is probably revoked, or the saved
                    // session disagrees with the local crypto store.
                    drop(client);
                    return recover_or_bail(
                        config,
                        state_dir,
                        recovery_attempts,
                        &format!(
                            "restore_session failed for device_id {saved_device_id}: {e}. \
                             The access token is likely revoked or the local crypto store is inconsistent."
                        ),
                    )
                    .await;
                }
            }

            let otk_corruption_flagged = client
                .state_store()
                .get_kv_data(matrix_sdk::store::StateStoreDataKey::OneTimeKeyAlreadyUploaded)
                .await
                .ok()
                .flatten()
                .is_some();
            if otk_corruption_flagged {
                drop(client);
                return recover_or_bail(
                    config,
                    state_dir,
                    recovery_attempts,
                    "matrix-sdk has flagged the local crypto store as out-of-sync with server-side one-time keys (StateStoreDataKey::OneTimeKeyAlreadyUploaded). The local store has lost track of OTKs that the server still records — fresh sends would fail to decrypt. The SDK has no in-place fix for this state.",
                )
                .await;
            }
        } else {
            login_fresh(&client, config).await?;
            if let Some(blob) = session_blob_from(&client)
                && let Err(e) = session::save(state_dir, &blob)
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "matrix: failed to persist session.json"
                );
            }
        }

        if let Some(key) = config.recovery_key.as_deref()
            && !key.is_empty()
        {
            run_recovery(&client, key).await;
        }

        Ok(client)
    }

    /// Either auto-wipe + retry (when password + user_id are configured) or
    /// bail with operator-actionable instructions.
    async fn recover_or_bail(
        config: &MatrixConfig,
        state_dir: &Path,
        recovery_attempts: u32,
        reason: &str,
    ) -> Result<Client> {
        if can_password_relogin(config) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "matrix: {reason} Auto-recovering: wiping {} and re-authenticating with password.",
                    state_dir.display()
                )
            );
            wipe_state(state_dir)?;
            return Box::pin(build_attempt(config, state_dir, recovery_attempts + 1)).await;
        }
        bail!(
            "matrix: {reason}\n\
             Cannot auto-recover because channels.matrix.password and channels.matrix.user-id are not both set.\n\
             Either:\n  \
             • configure channels.matrix.password (and user-id) so the next start can re-authenticate, or\n  \
             • wipe the state directory manually:  rm -rf {}",
            state_dir.display(),
        );
    }

    async fn login_fresh(client: &Client, config: &MatrixConfig) -> Result<()> {
        // Prefer password when set: it creates a server-side device matching
        // `config.device_id`, so subsequent crypto operations don't fight with
        // a token bound to a different device.
        if let Some(pw) = config.password.as_deref().filter(|s| !s.is_empty()) {
            return password_login(client, config, pw).await;
        }
        if config
            .access_token
            .as_deref()
            .is_some_and(|t| !t.is_empty())
        {
            return access_token_login(client, config).await;
        }
        bail!("matrix login requires either access_token or user_id+password")
    }

    async fn password_login(client: &Client, config: &MatrixConfig, password: &str) -> Result<()> {
        let user_id = config
            .user_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "matrix.user_id is required for password login"
                );
                anyhow::Error::msg("matrix.user_id is required for password login")
            })?;
        let mut login = client
            .matrix_auth()
            .login_username(&user_id, password)
            .initial_device_display_name("ZeroClaw");
        if let Some(d) = config.device_id.as_deref()
            && !d.is_empty()
        {
            login = login.device_id(d);
        }
        login.send().await.context("password login failed")?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "matrix: logged in via password"
        );
        Ok(())
    }

    async fn access_token_login(client: &Client, config: &MatrixConfig) -> Result<()> {
        let identity = resolve_access_token_identity(config).await?;
        let user_id = identity.user_id.parse().context("parse matrix.user_id")?;
        let device_id = identity.device_id.ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "matrix: access-token login requires a Matrix device_id"
            );
            anyhow::Error::msg("matrix: access-token login requires a Matrix device_id")
        })?;
        let session = MatrixSession {
            meta: SessionMeta {
                user_id,
                device_id: device_id.into(),
            },
            tokens: SessionTokens {
                access_token: config.access_token.clone().ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                        "matrix.access_token is required for token login"
                    );
                    anyhow::Error::msg("matrix.access_token is required for token login")
                })?,
                refresh_token: None,
            },
        };
        client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await
            .context("attach matrix session via access_token")?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "matrix: logged in via access_token"
        );
        Ok(())
    }

    fn non_empty_config_value(value: Option<&str>) -> Option<String> {
        value
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    }

    pub(super) async fn resolve_access_token_identity(
        config: &MatrixConfig,
    ) -> Result<AccessTokenIdentity> {
        let configured_user_id = non_empty_config_value(config.user_id.as_deref());
        let configured_device_id = non_empty_config_value(config.device_id.as_deref());

        if let (Some(user_id), Some(device_id)) =
            (configured_user_id.as_ref(), configured_device_id.as_ref())
        {
            return Ok(AccessTokenIdentity {
                user_id: user_id.clone(),
                device_id: Some(device_id.clone()),
            });
        }

        let whoami = fetch_access_token_whoami(config).await?;

        if let Some(ref configured) = configured_user_id
            && configured != &whoami.user_id
        {
            bail!(
                "matrix: configured channels.matrix.user-id ({configured}) does not match Matrix whoami user_id ({})",
                whoami.user_id
            );
        }

        if let (Some(configured), Some(actual)) = (&configured_device_id, &whoami.device_id)
            && configured != actual
        {
            bail!(
                "matrix: configured channels.matrix.device-id ({configured}) does not match Matrix whoami device_id ({actual})"
            );
        }

        if configured_device_id.is_none() && whoami.device_id.is_none() {
            bail!(
                "matrix: whoami response did not include device_id; configure channels.matrix.device-id for access-token login"
            );
        }

        Ok(AccessTokenIdentity {
            user_id: configured_user_id.unwrap_or(whoami.user_id),
            device_id: configured_device_id.or(whoami.device_id),
        })
    }

    async fn fetch_access_token_whoami(config: &MatrixConfig) -> Result<WhoamiResponse> {
        let access_token = config
            .access_token
            .as_deref()
            .context("matrix: whoami requires access_token")?;
        let url = matrix_client_api_url(&config.homeserver, WHOAMI_ENDPOINT)?;
        let response = reqwest::Client::builder()
            .timeout(WHOAMI_TIMEOUT)
            .build()
            .context("matrix: build whoami HTTP client")?
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .context("matrix: whoami request failed")?;
        let status = response.status();

        if !status.is_success() {
            let body = read_whoami_error_body_preview(response).await;
            bail!("matrix: whoami request failed with HTTP {status}: {body}");
        }

        let mut whoami = response
            .json::<WhoamiResponse>()
            .await
            .context("matrix: failed to parse whoami response")?;
        whoami.user_id = whoami.user_id.trim().to_string();
        if whoami.user_id.is_empty() {
            bail!("matrix: whoami response did not include user_id");
        }
        whoami.device_id = whoami
            .device_id
            .map(|device_id| device_id.trim().to_string())
            .filter(|device_id| !device_id.is_empty());

        Ok(whoami)
    }

    async fn read_whoami_error_body_preview(mut response: reqwest::Response) -> String {
        let mut preview = Vec::new();
        let mut truncated = false;

        while preview.len() < WHOAMI_ERROR_BODY_PREVIEW_BYTES {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => return format!("failed to read response body: {err}"),
            };
            let remaining = WHOAMI_ERROR_BODY_PREVIEW_BYTES - preview.len();
            if chunk.len() > remaining {
                preview.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            preview.extend_from_slice(&chunk);
        }

        if preview.len() == WHOAMI_ERROR_BODY_PREVIEW_BYTES {
            truncated = true;
        }

        format_whoami_error_body_preview(&preview, truncated)
    }

    fn format_whoami_error_body_preview(preview: &[u8], truncated: bool) -> String {
        if let Ok(error) = serde_json::from_slice::<MatrixErrorResponse>(preview) {
            let errcode = error
                .errcode
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let message = error
                .error
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let formatted = match (errcode, message) {
                (Some(errcode), Some(message)) => Some(format!("{errcode}: {message}")),
                (Some(errcode), None) => Some(errcode.to_string()),
                (None, Some(message)) => Some(message.to_string()),
                (None, None) => None,
            };
            if let Some(formatted) = formatted {
                return truncate_with_ellipsis(&formatted, WHOAMI_ERROR_BODY_DISPLAY_CHARS);
            }
        }

        let body = String::from_utf8_lossy(preview).trim().to_string();
        if body.is_empty() {
            return "<empty response body>".to_string();
        }
        let mut body = truncate_with_ellipsis(&body, WHOAMI_ERROR_BODY_DISPLAY_CHARS);
        if truncated {
            body.push_str(" [truncated]");
        }
        body
    }

    fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
        let mut chars = value.chars();
        let mut truncated: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            truncated.push_str("...");
        }
        truncated
    }

    fn matrix_client_api_url(homeserver: &str, endpoint_path: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(homeserver).context("parse matrix homeserver URL")?;
        let base_path = url.path().trim_end_matches('/');
        let endpoint_path = endpoint_path.trim_start_matches('/');
        let full_path = if base_path.is_empty() || base_path == "/" {
            format!("/{endpoint_path}")
        } else {
            format!("{base_path}/{endpoint_path}")
        };
        url.set_path(&full_path);
        url.set_query(None);
        url.set_fragment(None);
        Ok(url)
    }

    fn session_blob_from(client: &Client) -> Option<session::SessionBlob> {
        let session = client.matrix_auth().session()?;
        Some(session::SessionBlob {
            user_id: session.meta.user_id.to_string(),
            device_id: session.meta.device_id.to_string(),
            access_token: session.tokens.access_token,
            refresh_token: session.tokens.refresh_token,
        })
    }

    async fn run_recovery(client: &Client, key: &str) {
        use matrix_sdk::encryption::recovery::RecoveryState;

        let recovery = client.encryption().recovery();
        if matches!(recovery.state(), RecoveryState::Enabled) {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                "matrix: recovery already enabled, skipping recover()"
            );
            return;
        }

        let stripped_len = key.chars().filter(|c| !c.is_whitespace()).count();
        diagnose_secret_storage(client, stripped_len).await;

        match recovery.recover_and_fix_backup(key).await {
            Ok(()) => ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                "matrix: E2EE recovery completed (cross-signing + room keys imported; key backup repaired if inconsistent)"
            ),
            Err(e) => ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"e": e.to_string()})),
                "matrix: E2EE recovery failed: ; full error chain = . If the input length above is unexpected (base58 keys are typically ~58 chars, passphrases vary), the wrong value may be in channels.matrix.recovery-key."
            ),
        }
    }

    async fn diagnose_secret_storage(client: &Client, input_len: usize) {
        use matrix_sdk::ruma::events::secret_storage::{
            default_key::SecretStorageDefaultKeyEventContent, key::SecretStorageKeyEventContent,
        };
        use matrix_sdk::ruma::events::{GlobalAccountDataEventType, StaticEventContent};

        let account = client.account();
        let default_key = match account
            .fetch_account_data_static::<SecretStorageDefaultKeyEventContent>()
            .await
        {
            Ok(Some(raw)) => match raw.deserialize() {
                Ok(content) => Some(content),
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "matrix: cannot deserialize default secret-storage key event"
                    );
                    None
                }
            },
            Ok(None) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"input_len": input_len})),
                    "matrix: server has no m.secret_storage.default_key set; recovery cannot proceed (input_len=). Set up Secure Backup in Element first."
                );
                return;
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "matrix: failed to fetch default secret-storage key event"
                );
                return;
            }
        };
        let Some(default_key) = default_key else {
            return;
        };
        let key_id = default_key.key_id;

        // Fetch the actual key event for the default key id so we can see
        // whether it has passphrase info (affects which decode path the SDK
        // tries first inside SecretStorageKey::from_account_data).
        let event_type = GlobalAccountDataEventType::SecretStorageKey(key_id.clone());
        match account.fetch_account_data(event_type).await {
            Ok(Some(raw)) => {
                let json = raw.json().get();
                let has_passphrase =
                    json.contains("\"passphrase\"") && json.contains("\"iterations\"");
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!(
                        "matrix: secret-storage diagnostics: default_key_id={key_id}, \
                     has_passphrase_info={has_passphrase}, input_len={input_len}. \
                     {}",
                        if has_passphrase {
                            "SDK will try passphrase derivation first; if your input is a base58 key the passphrase MAC will fail and the error you see may be the passphrase error rather than the base58 fallback's error."
                        } else {
                            "SDK will use base58 decoding directly."
                        }
                    )
                );
                let _ = SecretStorageKeyEventContent::TYPE; // keep import live
            }
            Ok(None) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"key_id": key_id})),
                    "matrix: default key id has no corresponding key event on the account — secret storage is in an inconsistent state. Re-running Secure Backup setup in Element will repair this."
                );
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"error": format!("{}", e), "key_id": key_id})
                        ),
                    "matrix: failed to fetch key event for"
                );
            }
        }
    }

    pub(super) fn normalize_recipient(id_or_alias: &str) -> (&str, bool) {
        if !id_or_alias.contains("||") {
            return (id_or_alias, false);
        }
        let chosen = id_or_alias
            .split("||")
            .map(str::trim)
            .filter(|s| s.starts_with('!') || s.starts_with('#'))
            .last()
            .unwrap_or(id_or_alias);
        (chosen, true)
    }

    pub(super) async fn resolve_room(
        client: &Client,
        cache: &Arc<RwLock<HashMap<String, OwnedRoomId>>>,
        id_or_alias: &str,
    ) -> Result<OwnedRoomId> {
        let (id_or_alias, normalized) = normalize_recipient(id_or_alias);
        if normalized {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"id_or_alias": id_or_alias})),
                "matrix: recipient contains `||`; using as the room target. Update channels.matrix or cron `delivery.to` to a plain room id/alias to silence this warning."
            );
        }
        if id_or_alias.starts_with('!') {
            return id_or_alias
                .parse::<matrix_sdk::ruma::OwnedRoomId>()
                .with_context(|| format!("parse room id {id_or_alias}"));
        }
        if !id_or_alias.starts_with('#') {
            bail!("matrix: not a room id or alias: {id_or_alias}");
        }
        if let Some(id) = cache.read().await.get(id_or_alias) {
            return Ok(id.clone());
        }
        let alias: &RoomAliasId = id_or_alias
            .try_into()
            .with_context(|| format!("parse room alias {id_or_alias}"))?;
        let resp = client
            .resolve_room_alias(alias)
            .await
            .with_context(|| format!("resolve room alias {id_or_alias}"))?;
        cache
            .write()
            .await
            .insert(id_or_alias.to_string(), resp.room_id.clone());
        Ok(resp.room_id)
    }
}

// ─── inbound ───────────────────────────────────────────────────────────────
mod inbound {
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, SystemTime},
    };

    use matrix_sdk::{
        Client, Room, RoomState,
        config::SyncSettings,
        event_handler::RawEvent,
        ruma::{
            OwnedEventId, OwnedUserId,
            events::{
                AnySyncTimelineEvent,
                reaction::ReactionEventContent,
                relation::Annotation,
                room::{
                    encrypted::OriginalSyncRoomEncryptedEvent,
                    message::{MessageType, OriginalSyncRoomMessageEvent},
                },
            },
            serde::Raw,
        },
    };
    use serde_json::Value as JsonValue;
    use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock, mpsc, oneshot};

    use super::{allowlist, approval, context as ctx_mod, mention};
    use crate::transcription::TranscriptionManager;
    use zeroclaw_api::{
        channel::{ChannelApprovalResponse, ChannelMessage},
        media::MediaAttachment,
    };
    use zeroclaw_config::schema::{MatrixConfig, TranscriptionConfig};

    pub(super) const SYNC_LONGPOLL_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Clone)]
    pub(super) struct HandlerCtx {
        pub config: Arc<MatrixConfig>,
        /// ZeroClaw alias for `[channels.matrix.<alias>]` so session_key
        /// construction can scope by bot instance.
        pub alias: String,
        /// Resolves inbound external peers from canonical state at message-time.
        /// No cache (see AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH").
        pub peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        pub transcription: Option<Arc<TranscriptionConfig>>,
        pub workspace_dir: Option<Arc<std::path::PathBuf>>,
        pub tx: mpsc::Sender<ChannelMessage>,
        pub pending_approvals:
            Arc<TokioMutex<HashMap<String, oneshot::Sender<ChannelApprovalResponse>>>>,
        pub threads_seen: Arc<TokioRwLock<HashSet<OwnedEventId>>>,
        pub bot_user_id: OwnedUserId,
        pub bot_display_name: Arc<TokioRwLock<Option<String>>>,
        pub initial_sync_done: Arc<AtomicBool>,
        /// Event ids of inbound events that arrived as `m.room.encrypted` and
        /// could not be decrypted. Tracked so the bot reacts ❓ exactly once
        /// per event across sync catchup deliveries.
        pub undecryptable_seen: Arc<TokioMutex<HashSet<OwnedEventId>>>,
    }

    pub(super) async fn run_sync_loop(client: Client, ctx: HandlerCtx) -> anyhow::Result<()> {
        let handler_ctx = ctx.clone();
        let message_handler = client.add_event_handler(
            move |ev: OriginalSyncRoomMessageEvent, room: Room, raw: RawEvent| {
                let ctx = handler_ctx.clone();
                async move {
                    if let Err(e) = handle_message(ctx, ev, room, raw).await {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "matrix: handle_message failed"
                        );
                    }
                }
            },
        );
        let _message_handler_guard = client.event_handler_drop_guard(message_handler);

        // Surface inbound events the SDK couldn't decrypt by reacting ❓ on
        // the encrypted event so the operator notices a key gap in chat
        // instead of silent dropping. Best-effort: prophylactic in normally-
        // healthy rooms where decryption succeeds.
        let encrypted_ctx = ctx.clone();
        let encrypted_handler =
            client.add_event_handler(move |ev: OriginalSyncRoomEncryptedEvent, room: Room| {
                let ctx = encrypted_ctx.clone();
                async move {
                    handle_undecryptable(ctx, ev, room).await;
                }
            });
        let _encrypted_handler_guard = client.event_handler_drop_guard(encrypted_handler);

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "matrix: starting sync loop"
        );
        let sync_settings = SyncSettings::default().timeout(SYNC_LONGPOLL_TIMEOUT);
        if let Err(e) = client.sync_once(sync_settings.clone()).await {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "phase": "initial_sync",
                        "error": format!("{}", e),
                    })),
                "matrix: initial sync failed"
            );
            return Err(anyhow::Error::msg(format!(
                "matrix initial sync failed: {e}"
            )));
        }
        ctx.initial_sync_done.store(true, Ordering::SeqCst);
        client.sync(sync_settings).await.map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "phase": "sync_loop",
                        "error": format!("{}", e),
                    })),
                "matrix: sync loop failed"
            );
            anyhow::Error::msg(format!("matrix sync loop failed: {e}"))
        })
    }

    /// React ❓ on any inbound event the SDK delivered as still-encrypted
    /// (decryption failed or no keys available). Skips the bot's own
    /// events, non-Joined rooms, and any event already reacted to in this
    /// process. Reaction send failures are warn-logged, not propagated.
    async fn handle_undecryptable(ctx: HandlerCtx, ev: OriginalSyncRoomEncryptedEvent, room: Room) {
        if room.state() != RoomState::Joined {
            return;
        }
        if ev.sender == ctx.bot_user_id {
            return;
        }
        let event_id = ev.event_id.clone();
        let already = {
            let mut seen = ctx.undecryptable_seen.lock().await;
            !seen.insert(event_id.clone())
        };
        if already {
            return;
        }
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "matrix: reacting ❓ to undecryptable event {} from {}",
                event_id, ev.sender
            )
        );
        let content =
            ReactionEventContent::new(Annotation::new(event_id.clone(), "❓".to_string()));
        if let Err(e) = room.send(content).await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(
                        ::serde_json::json!({"error": format!("{}", e), "event_id": event_id})
                    ),
                "matrix: failed to react ❓ on undecryptable event"
            );
        }
    }

    async fn handle_message(
        ctx: HandlerCtx,
        ev: OriginalSyncRoomMessageEvent,
        room: Room,
        raw: RawEvent,
    ) -> anyhow::Result<()> {
        if room.state() != RoomState::Joined {
            return Ok(());
        }
        if ev.sender == ctx.bot_user_id {
            return Ok(());
        }

        let body = ctx_mod::body_for(&ev.content.msgtype);
        let sender = ev.sender.as_str();
        let room_id = room.room_id().as_str();

        // Approval reply has highest priority — operator answer must work even
        // if the room/user filters would otherwise drop the message.
        if let Some((token, response)) = approval::parse_reply(&body) {
            let waiter = ctx.pending_approvals.lock().await.remove(&token);
            if let Some(tx) = waiter {
                let _ = tx.send(response);
                return Ok(());
            }
        }

        let allowed_peers = (ctx.peer_resolver)();
        if !allowlist::user_allowed(&allowed_peers, sender) {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"sender": sender})),
                "matrix: drop message from non-allowed sender"
            );
            return Ok(());
        }
        if !allowlist::room_allowed_static(&ctx.config.allowed_rooms, room_id) {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"room_id": room_id})),
                "matrix: drop message from non-allowed room"
            );
            return Ok(());
        }

        if ctx.config.mention_only && is_group_room(&room).await {
            let display_name = ctx.bot_display_name.read().await.clone();
            let mention_user_ids = extract_mentions_user_ids(&raw);
            if !mention::is_mentioned(
                &ctx.bot_user_id,
                display_name.as_deref(),
                mention_user_ids.as_deref(),
                &body,
            ) {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"sender": sender})),
                    "matrix: drop unmentioned message from"
                );
                return Ok(());
            }
        }

        let thread_id = extract_thread_id(&raw);
        let mut content = body.clone();
        if let Some(tid) = thread_id.as_ref()
            && ctx_mod::claim_first_visit(&ctx.threads_seen, tid).await
        {
            match room.event(tid, None).await {
                Ok(timeline_event) => {
                    if let Some((root_sender, root_body)) =
                        extract_root_summary(timeline_event.into_raw())
                    {
                        content = format!(
                            "{}{}",
                            ctx_mod::format_preamble(&root_sender, &root_body),
                            content
                        );
                    }
                }
                Err(e) => ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e), "tid": tid})),
                    "matrix: failed to fetch thread root"
                ),
            }
        }

        let media_kind = match &ev.content.msgtype {
            MessageType::Image(m) => Some(MediaInfo::new(
                m.source.clone(),
                m.body.clone(),
                m.info.as_ref().and_then(|i| i.mimetype.clone()),
                MediaCategory::Image,
            )),
            MessageType::File(m) => Some(MediaInfo::new(
                m.source.clone(),
                m.body.clone(),
                m.info.as_ref().and_then(|i| i.mimetype.clone()),
                MediaCategory::File,
            )),
            MessageType::Video(m) => Some(MediaInfo::new(
                m.source.clone(),
                m.body.clone(),
                m.info.as_ref().and_then(|i| i.mimetype.clone()),
                MediaCategory::Video,
            )),
            MessageType::Audio(m) => {
                let kind = if is_voice_message(&raw) {
                    MediaCategory::Voice
                } else {
                    MediaCategory::Audio
                };
                Some(MediaInfo::new(
                    m.source.clone(),
                    m.body.clone(),
                    m.info.as_ref().and_then(|i| i.mimetype.clone()),
                    kind,
                ))
            }
            _ => None,
        };

        if let Some(info) = media_kind {
            content = attach_media(
                &room,
                &info,
                ctx.workspace_dir.as_deref(),
                &body,
                content,
                ctx.transcription.as_deref(),
            )
            .await;
        } else if let Some(reply_target) = extract_in_reply_to(&raw) {
            match room.event(&reply_target, None).await {
                Ok(timeline_event) => {
                    if let Some(info) = parent_media_info(timeline_event.into_raw()) {
                        content = attach_media(
                            &room,
                            &info,
                            ctx.workspace_dir.as_deref(),
                            "",
                            content,
                            ctx.transcription.as_deref(),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"error": format!("{}", e), "reply_target": reply_target})), "matrix: could not fetch in_reply_to parent")
                }
            }
        }
        let attachments: Vec<MediaAttachment> = Vec::new();

        let outbound_anchor =
            resolve_outbound_anchor(thread_id.as_ref(), &ev.event_id, ctx.config.reply_in_thread);
        // When the bot is the one starting the thread, mark its root seen
        // so the next inbound that lands inside it does not re-fetch and
        // re-inject a root preamble (the agent already saw the root in this
        // same turn).
        if thread_id.is_none() && ctx.config.reply_in_thread {
            ctx_mod::mark_seen(&ctx.threads_seen, ev.event_id.clone()).await;
        }

        let interruption_scope =
            interruption_scope_from_anchor(outbound_anchor.as_deref(), &ev.event_id);

        let msg = ChannelMessage {
            id: ev.event_id.to_string(),
            sender: sender.to_string(),
            reply_target: room.room_id().to_string(),
            content,
            channel: "matrix".to_string(),
            channel_alias: Some(ctx.alias.clone()),
            timestamp: SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: outbound_anchor.clone(),
            interruption_scope_id: interruption_scope,
            attachments,
            subject: None,

            ..Default::default()
        };

        if let Err(e) = ctx.tx.send(msg).await {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "matrix: failed to forward inbound message"
            );
        }
        Ok(())
    }

    async fn is_group_room(room: &Room) -> bool {
        !matches!(room.is_direct().await, Ok(true))
    }

    pub(super) fn extract_mentions_user_ids(raw: &RawEvent) -> Option<Vec<String>> {
        let v: JsonValue = serde_json::from_str(raw.get()).ok()?;
        let mentions = v.get("content")?.get("m.mentions")?;
        let arr = mentions.get("user_ids")?.as_array()?;
        Some(
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect(),
        )
    }

    pub(super) fn resolve_outbound_anchor(
        thread_id: Option<&OwnedEventId>,
        event_id: &OwnedEventId,
        reply_in_thread: bool,
    ) -> Option<String> {
        thread_id.map(ToString::to_string).or_else(|| {
            if reply_in_thread {
                Some(event_id.to_string())
            } else {
                None
            }
        })
    }

    pub(super) fn interruption_scope_from_anchor(
        outbound_anchor: Option<&str>,
        event_id: &OwnedEventId,
    ) -> Option<String> {
        match outbound_anchor {
            Some(anchor) if anchor == event_id.as_str() => None,
            other => other.map(ToString::to_string),
        }
    }

    pub(super) fn extract_thread_id(raw: &RawEvent) -> Option<OwnedEventId> {
        let v: JsonValue = serde_json::from_str(raw.get()).ok()?;
        let relates = v.get("content")?.get("m.relates_to")?;
        let rel_type = relates.get("rel_type")?.as_str()?;
        if rel_type != "m.thread" {
            return None;
        }
        let root = relates.get("event_id")?.as_str()?;
        root.parse().ok()
    }

    pub(super) fn extract_in_reply_to(raw: &RawEvent) -> Option<OwnedEventId> {
        let v: JsonValue = serde_json::from_str(raw.get()).ok()?;
        let relates = v.get("content")?.get("m.relates_to")?;
        let in_reply_to = relates.get("m.in_reply_to")?;
        let event_id = in_reply_to.get("event_id")?.as_str()?;
        event_id.parse().ok()
    }

    pub(super) fn is_voice_message(raw: &RawEvent) -> bool {
        let v: JsonValue = match serde_json::from_str(raw.get()) {
            Ok(v) => v,
            Err(_) => return false,
        };
        v.get("content")
            .and_then(|c| c.get("org.matrix.msc3245.voice"))
            .is_some()
    }

    fn extract_root_summary(raw: Raw<AnySyncTimelineEvent>) -> Option<(String, String)> {
        let json: JsonValue = serde_json::from_str(raw.json().get()).ok()?;
        let sender = json.get("sender")?.as_str()?.to_string();
        let body = json
            .get("content")
            .and_then(|c| c.get("body"))
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string();
        Some((sender, body))
    }

    pub(super) enum MediaCategory {
        Image,
        Video,
        Audio,
        Voice,
        File,
    }

    pub(super) fn should_transcribe(
        kind: &MediaCategory,
        transcription: Option<&TranscriptionConfig>,
    ) -> bool {
        matches!(kind, MediaCategory::Voice) && matches!(transcription, Some(t) if t.enabled)
    }

    async fn attach_media(
        room: &Room,
        info: &MediaInfo,
        workspace_dir: Option<&std::path::PathBuf>,
        body_hint: &str,
        content: String,
        transcription: Option<&TranscriptionConfig>,
    ) -> String {
        let mut content = content;
        match save_media_to_workspace(room, info, workspace_dir).await {
            Ok(Some(path)) => {
                let marker = format_media_marker(info, &path);
                let placeholder = matches!(body_hint, "[image]" | "[file]" | "[audio]" | "[video]");
                content = if body_hint.is_empty() {
                    if content.is_empty() {
                        marker
                    } else {
                        format!("{content}\n\n{marker}")
                    }
                } else if placeholder || body_hint == info.file_name || content == body_hint {
                    marker
                } else {
                    format!("{content}\n\n{marker}")
                };

                if should_transcribe(&info.kind, transcription) {
                    let t = transcription.expect("should_transcribe guarantees Some");
                    match transcribe_from_disk(t, &path, &info.file_name).await {
                        Ok(text) if !text.trim().is_empty() => {
                            content = format!("[voice transcript]: {text}\n\n{content}");
                        }
                        Ok(_) => {}
                        Err(e) => ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "matrix: voice transcription failed"
                        ),
                    }
                }
            }
            Ok(None) => {}
            Err(e) => ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "matrix: media handling failed"
            ),
        }
        content
    }

    /// Walk a fetched timeline event's raw JSON looking for a media-typed
    /// `m.room.message` payload. Returns `None` if the event is not a
    /// recognized media message.
    pub(super) fn parent_media_info(
        raw: matrix_sdk::ruma::serde::Raw<matrix_sdk::ruma::events::AnySyncTimelineEvent>,
    ) -> Option<MediaInfo> {
        let json: JsonValue = serde_json::from_str(raw.json().get()).ok()?;
        let content = json.get("content")?;
        let msgtype = content.get("msgtype")?.as_str()?;
        let kind = match msgtype {
            "m.image" => MediaCategory::Image,
            "m.video" => MediaCategory::Video,
            "m.audio" if content.get("org.matrix.msc3245.voice").is_some() => MediaCategory::Voice,
            "m.audio" => MediaCategory::Audio,
            "m.file" => MediaCategory::File,
            _ => return None,
        };
        let file_name = content
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("attachment")
            .to_string();
        let mime = content
            .get("info")
            .and_then(|i| i.get("mimetype"))
            .and_then(|m| m.as_str())
            .map(String::from);
        let source = if let Some(file) = content.get("file") {
            // Encrypted media: rebuild MediaSource::Encrypted from JSON.
            let encrypted: matrix_sdk::ruma::events::room::EncryptedFile =
                serde_json::from_value(file.clone()).ok()?;
            matrix_sdk::ruma::events::room::MediaSource::Encrypted(Box::new(encrypted))
        } else if let Some(url) = content.get("url").and_then(|u| u.as_str()) {
            matrix_sdk::ruma::events::room::MediaSource::Plain(matrix_sdk::ruma::OwnedMxcUri::from(
                url,
            ))
        } else {
            return None;
        };
        Some(MediaInfo::new(source, file_name, mime, kind))
    }

    pub(super) struct MediaInfo {
        pub source: matrix_sdk::ruma::events::room::MediaSource,
        pub file_name: String,
        pub mime: Option<String>,
        pub kind: MediaCategory,
    }

    impl MediaInfo {
        pub fn new(
            source: matrix_sdk::ruma::events::room::MediaSource,
            file_name: String,
            mime: Option<String>,
            kind: MediaCategory,
        ) -> Self {
            Self {
                source,
                file_name,
                mime,
                kind,
            }
        }
    }

    /// Download an inbound media file, persist it to `{workspace}/matrix_files/`,
    /// and return the on-disk path. Returns `Ok(None)` when no `workspace_dir`
    /// is configured (caller logs and falls back to the placeholder body).
    async fn save_media_to_workspace(
        room: &Room,
        info: &MediaInfo,
        workspace: Option<&std::path::PathBuf>,
    ) -> anyhow::Result<Option<std::path::PathBuf>> {
        let Some(workspace) = workspace else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                &format!(
                    "matrix: cannot persist {} — channels.matrix workspace_dir not configured. Set ZEROCLAW_DIR or run via the orchestrator.",
                    info.file_name
                )
            );
            return Ok(None);
        };
        let dir = workspace.join("matrix_files");
        std::fs::create_dir_all(&dir).map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": dir.display().to_string(),
                        "phase": "media_dir_create",
                        "error": format!("{}", e),
                    })),
                "matrix: failed to create media dir"
            );
            anyhow::Error::msg(format!("create {}: {e}", dir.display()))
        })?;
        let request = matrix_sdk::media::MediaRequestParameters {
            source: info.source.clone(),
            format: matrix_sdk::media::MediaFormat::File,
        };
        let source_kind = match &info.source {
            matrix_sdk::ruma::events::room::MediaSource::Plain(_) => "plain",
            matrix_sdk::ruma::events::room::MediaSource::Encrypted(_) => "encrypted",
        };
        let bytes = room
            .client()
            .media()
            .get_media_content(&request, true)
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "get_media_content ()"
                );
                anyhow::Error::msg(format!("get_media_content ({source_kind}): {e}"))
            })?;

        let safe_name = sanitize_filename(&info.file_name, &info.kind, info.mime.as_deref());
        // Disambiguate by uuid prefix to avoid collisions across messages.
        let unique = format!("{}_{safe_name}", uuid::Uuid::new_v4().simple());
        let path = dir.join(unique);
        std::fs::write(&path, &bytes).map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": path.display().to_string(),
                        "phase": "media_write",
                        "error": format!("{}", e),
                    })),
                "matrix: failed to write media file"
            );
            anyhow::Error::msg(format!("write {}: {e}", path.display()))
        })?;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "matrix: saved {} bytes ({}) to {}",
                bytes.len(),
                source_kind,
                path.display()
            )
        );
        Ok(Some(path))
    }

    fn sanitize_filename(raw: &str, kind: &MediaCategory, mime: Option<&str>) -> String {
        let trimmed = raw.trim();
        let candidate = if trimmed.is_empty() || trimmed.starts_with('[') {
            // Placeholder body or empty — synthesise a sensible name.
            let ext = default_extension(kind, mime);
            format!("matrix_media.{ext}")
        } else {
            trimmed.to_string()
        };
        candidate
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn default_extension(kind: &MediaCategory, mime: Option<&str>) -> &'static str {
        if let Some(m) = mime {
            match m {
                "image/png" => return "png",
                "image/jpeg" | "image/jpg" => return "jpg",
                "image/gif" => return "gif",
                "image/webp" => return "webp",
                "video/mp4" => return "mp4",
                "audio/ogg" => return "ogg",
                "audio/mpeg" | "audio/mp3" => return "mp3",
                "audio/wav" => return "wav",
                "application/pdf" => return "pdf",
                _ => {}
            }
        }
        match kind {
            MediaCategory::Image => "jpg",
            MediaCategory::Video => "mp4",
            MediaCategory::Audio | MediaCategory::Voice => "ogg",
            MediaCategory::File => "bin",
        }
    }

    fn format_media_marker(info: &MediaInfo, path: &std::path::Path) -> String {
        match info.kind {
            MediaCategory::Image => format!("[IMAGE:{}]", path.display()),
            _ => {
                let display_name = if info.file_name.trim().is_empty() {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("attachment")
                        .to_string()
                } else {
                    info.file_name.clone()
                };
                format!("[Document: {display_name}] {}", path.display())
            }
        }
    }

    async fn transcribe_from_disk(
        config: &TranscriptionConfig,
        path: &std::path::Path,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let bytes = std::fs::read(path).map_err(|e| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "path": path.display().to_string(),
                        "phase": "transcription_read",
                        "error": format!("{}", e),
                    })),
                "matrix: failed to read media file for transcription"
            );
            anyhow::Error::msg(format!("read {}: {e}", path.display()))
        })?;
        let manager = TranscriptionManager::new(config)?;
        manager.transcribe(&bytes, file_name).await
    }
}

// ─── outbound ──────────────────────────────────────────────────────────────
mod outbound {
    use std::{collections::HashMap, sync::Arc};

    use anyhow::{Context as _, Result, bail};
    use futures_util::StreamExt;
    use matrix_sdk::{
        Client, Room, RoomState,
        attachment::{
            AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo,
            BaseVideoInfo,
        },
        room::{
            edit::EditedContent,
            reply::{EnforceThread, Reply},
        },
        ruma::{
            OwnedEventId, OwnedRoomId, UInt,
            events::{
                reaction::ReactionEventContent,
                relation::Annotation,
                room::message::{
                    AddMentions, MessageType, ReplyWithinThread, RoomMessageEventContent,
                    RoomMessageEventContentWithoutRelation, TextMessageEventContent,
                },
            },
        },
    };
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::Duration;
    use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock};

    use super::{client, context as ctx_mod, markers};
    use zeroclaw_api::{channel::SendMessage, media::MediaAttachment};

    pub(super) type ReactionKey = (OwnedRoomId, OwnedEventId, String);

    pub(super) struct Outbox<'a> {
        pub client: &'a Client,
        pub alias_cache: &'a Arc<TokioRwLock<HashMap<String, OwnedRoomId>>>,
        pub threads_seen: &'a Arc<TokioRwLock<std::collections::HashSet<OwnedEventId>>>,
        pub reaction_log: &'a Arc<TokioMutex<HashMap<ReactionKey, OwnedEventId>>>,
        pub reply_in_thread: bool,
        pub workspace_dir: Option<&'a Path>,
    }

    /// What `outbound::send` should do once all attachment uploads are done
    /// and the marker-stripped text is in hand. Extracted as a small enum so
    /// the empty-text-with-attachments contract can be unit-tested without
    /// the SDK in the loop.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum SendOutcome {
        /// Text is non-empty (with or without prior attachments). Caller
        /// proceeds to send the text message and returns its event_id.
        SendText,
        /// Text is empty but at least one attachment uploaded successfully.
        /// Caller skips the text send and returns the carried event_id.
        ReturnAttachment,
        /// Text is empty AND no attachment landed. Caller surfaces an error
        /// to the runtime so it can decide what to do.
        EmptyError,
    }

    /// Decide what `outbound::send` should do given the post-marker-strip
    /// text and whether at least one attachment landed. Pure function.
    pub(super) fn decide_send_outcome(
        text_is_empty_after_strip: bool,
        any_attachment_landed: bool,
    ) -> SendOutcome {
        match (text_is_empty_after_strip, any_attachment_landed) {
            (false, _) => SendOutcome::SendText,
            (true, true) => SendOutcome::ReturnAttachment,
            (true, false) => SendOutcome::EmptyError,
        }
    }

    /// Why a marker upload didn't reach the room. Drives both the textual
    /// "(note: I couldn't deliver…)" line and the emoji reactions on the
    /// agent's outgoing message so a chatter sees a hard refusal at a glance.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum MarkerFailure {
        /// Trust-boundary refusal: `validate_marker_target` rejected the
        /// target (path escapes workspace, disallowed scheme, etc.). The bot
        /// deliberately did not attempt the fetch.
        Refused,
        /// Post-validation failure: fetch error, file not found, upload
        /// rejected by the server, oversize body, timeout. The bot tried and
        /// couldn't complete the delivery.
        Failed,
    }

    pub(super) struct AttachmentDelivery {
        pub text: String,
        pub last_attachment_id: Option<OwnedEventId>,
        pub failed_markers: Vec<(String, MarkerFailure)>,
    }

    impl AttachmentDelivery {
        pub(super) fn failure_kinds(&self) -> Vec<MarkerFailure> {
            self.failed_markers.iter().map(|(_, kind)| *kind).collect()
        }
    }

    /// Pick the emoji reactions to apply to the agent's outgoing text/event
    /// based on which kinds of marker failures occurred. 🚫 means the bot
    /// refused for safety; ⚠️ means it tried and didn't make it. Both can
    /// fire on the same message when a batch mixes refusals and failures.
    pub(super) fn decide_reactions(failures: &[MarkerFailure]) -> Vec<&'static str> {
        let mut out = Vec::new();
        if failures.iter().any(|f| matches!(f, MarkerFailure::Refused)) {
            out.push("🚫");
        }
        if failures.iter().any(|f| matches!(f, MarkerFailure::Failed)) {
            out.push("⚠️");
        }
        out
    }

    /// 8 MiB cap on the body of an HTTP marker fetch. Matches WebFetchTool's
    /// streaming-cap pattern in `crates/zeroclaw-tools/src/web_fetch.rs`.
    const MAX_MARKER_BYTES: usize = 8 * 1024 * 1024;
    /// 30-second connect+request timeout for HTTP marker fetches. Bounds the
    /// agent-driven fetch path so a hung target cannot stall the channel.
    const MARKER_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

    /// Resolved marker fetch target after sandboxing. `Local` paths are
    /// canonicalised and proven to live within the configured `workspace_dir`.
    /// `Http` URLs have an explicit `http`/`https` scheme.
    #[derive(Debug)]
    pub(super) enum MarkerTarget {
        Local(PathBuf),
        Http(reqwest::Url),
    }

    #[derive(Debug)]
    pub(super) enum ValidateError {
        /// Trust-boundary refusal: disallowed scheme, no workspace
        /// configured, or path resolved outside the workspace. The target
        /// was a real, reachable resource that policy declined.
        Refused(anyhow::Error),
        /// The path didn't resolve to anything on disk (ENOENT or similar
        /// during canonicalize). Treated as a delivery failure, not a
        /// safety event.
        NotFound(anyhow::Error),
    }

    impl std::fmt::Display for ValidateError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                ValidateError::Refused(e) | ValidateError::NotFound(e) => write!(f, "{e}"),
            }
        }
    }

    impl ValidateError {
        pub(super) fn as_marker_failure(&self) -> MarkerFailure {
            match self {
                ValidateError::Refused(_) => MarkerFailure::Refused,
                ValidateError::NotFound(_) => MarkerFailure::Failed,
            }
        }
    }

    pub(super) fn validate_marker_target(
        target: &str,
        workspace_dir: Option<&Path>,
    ) -> std::result::Result<MarkerTarget, ValidateError> {
        if target.starts_with("http://") || target.starts_with("https://") {
            let url = reqwest::Url::parse(target)
                .with_context(|| format!("parse marker URL {target}"))
                .map_err(ValidateError::Refused)?;
            let host_str = url.host_str().unwrap_or("");
            if zeroclaw_tools::helpers::domain_guard::is_private_or_local_host(host_str) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "target": target,
                            "host": host_str,
                            "reason": "ssrf_private_host",
                        })),
                    "matrix: marker target points to a private/local host"
                );
                return Err(ValidateError::Refused(anyhow::Error::msg(format!(
                    "matrix: marker target {target} resolves to a private or local host ({host_str}); refusing for SSRF safety. \
                     Use a public URL or attach the file from workspace_dir directly."
                ))));
            }
            return Ok(MarkerTarget::Http(url));
        }
        if target.contains("://") {
            let scheme = target.split("://").next().unwrap_or("?");
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "scheme": scheme,
                        "target": target,
                    })),
                "matrix: marker target uses disallowed scheme"
            );
            return Err(ValidateError::Refused(anyhow::Error::msg(format!(
                "matrix: marker target uses disallowed scheme {scheme:?}; only http/https and workspace-relative paths are accepted"
            ))));
        }
        if target.starts_with("data:") || target.starts_with("file:") {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "target": target,
                    })),
                "matrix: marker target uses disallowed data: or file: scheme"
            );
            return Err(ValidateError::Refused(anyhow::Error::msg(
                "matrix: marker target uses disallowed scheme; only http/https and workspace-relative paths are accepted",
            )));
        }

        let workspace = workspace_dir.ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "target": target,
                        "reason": "no_workspace_dir",
                    })),
                "matrix: marker target is local path but channel has no workspace_dir"
            );
            ValidateError::Refused(anyhow::Error::msg(format!(
                "matrix: marker target {target} is a local path but the channel was started without a workspace_dir, refusing for safety"
            )))
        })?;
        let workspace_canon = std::fs::canonicalize(workspace)
            .with_context(|| format!("canonicalize workspace {}", workspace.display()))
            .map_err(ValidateError::Refused)?;

        let target_path = Path::new(target);
        let absolute = if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            workspace_canon.join(target_path)
        };
        let target_canon = match std::fs::canonicalize(&absolute) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "target": target,
                            "reason": "not_found",
                        })),
                    "matrix: marker target not found on disk"
                );
                return Err(ValidateError::NotFound(anyhow::Error::msg(format!(
                    "matrix: marker target {target} not found on disk"
                ))));
            }
            Err(e) => {
                return Err(ValidateError::Refused(
                    anyhow::Error::from(e).context(format!("canonicalize marker target {target}")),
                ));
            }
        };

        if !target_canon.starts_with(&workspace_canon) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "target": target,
                        "target_canon": target_canon.display().to_string(),
                        "workspace_canon": workspace_canon.display().to_string(),
                        "reason": "outside_workspace",
                    })),
                "matrix: marker target escapes workspace_dir"
            );
            return Err(ValidateError::Refused(anyhow::Error::msg(format!(
                "matrix: marker target {target} resolves to {} which is outside workspace_dir {}; refusing",
                target_canon.display(),
                workspace_canon.display(),
            ))));
        }
        Ok(MarkerTarget::Local(target_canon))
    }

    /// Maximum number of redirects to follow on a marker HTTP fetch. The
    /// `Policy::custom` closure below rejects any redirect target whose host
    /// is private/local, so the cap mainly bounds the worst-case public-→-public
    /// chain an attacker can construct.
    const MAX_MARKER_REDIRECTS: usize = 10;

    fn marker_http_client() -> &'static reqwest::Client {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        CLIENT.get_or_init(|| {
            let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_MARKER_REDIRECTS {
                    return attempt.error(std::io::Error::other(format!(
                        "Too many marker redirects (max {MAX_MARKER_REDIRECTS})"
                    )));
                }
                // `attempt.url()` borrows the attempt, so we copy out the
                // bits we need into owned Strings before `attempt.error(...)`,
                // which moves the attempt, can run.
                let target_str = attempt.url().as_str().to_string();
                let host = attempt.url().host_str().unwrap_or("").to_string();
                if zeroclaw_tools::helpers::domain_guard::is_private_or_local_host(&host) {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "target": target_str,
                                "host": host,
                                "reason": "ssrf_redirect_to_private_host",
                            })),
                        "matrix: marker redirect targets a private/local host"
                    );
                    return attempt.error(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "Blocked marker redirect to private or local host ({host}); \
                             refusing for SSRF safety. Use a public URL or attach the file \
                             from workspace_dir directly."
                        ),
                    ));
                }
                attempt.follow()
            });
            reqwest::Client::builder()
                .timeout(MARKER_HTTP_TIMEOUT)
                .redirect(redirect_policy)
                .user_agent("zeroclaw-matrix/1.0")
                .build()
                .expect("default reqwest client config never fails to build")
        })
    }

    pub(super) async fn fetch_http(url: reqwest::Url) -> Result<Vec<u8>> {
        let client = marker_http_client();
        let resp = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("fetch marker URL {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("matrix: marker URL {url} returned HTTP status {status}");
        }
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("stream chunk from {url}"))?;
            if buf.len().saturating_add(chunk.len()) > MAX_MARKER_BYTES {
                bail!("matrix: marker URL {url} exceeded {MAX_MARKER_BYTES}-byte cap; refusing");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    pub(super) fn thread_anchor_from_message(
        outbox: &Outbox<'_>,
        message: &SendMessage,
    ) -> Option<OwnedEventId> {
        if outbox.reply_in_thread {
            message
                .thread_ts
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse().ok())
        } else {
            None
        }
    }

    pub(super) async fn deliver_attachments(
        outbox: &Outbox<'_>,
        room: &Room,
        mut text: String,
        markers: &[markers::Marker],
        attachments: &[MediaAttachment],
        thread_anchor: Option<&OwnedEventId>,
    ) -> Result<AttachmentDelivery> {
        let mut last_attachment_id: Option<OwnedEventId> = None;
        for att in attachments {
            let id = upload_attachment(room, att, AttachmentKind::Auto, thread_anchor).await?;
            last_attachment_id = Some(id);
        }

        // Track each failed marker with the reason: Refused (trust-boundary
        // rejection by validate_marker_target) vs Failed (everything else —
        // fetch error, upload rejection). Drives both the textual note and
        // the emoji reactions fired below.
        let mut failed_markers: Vec<(String, MarkerFailure)> = Vec::new();
        for marker in markers {
            let kind = match marker.kind {
                markers::MarkerKind::Image => AttachmentKind::Image,
                markers::MarkerKind::Audio => AttachmentKind::Audio,
                markers::MarkerKind::Video => AttachmentKind::Video,
                markers::MarkerKind::File => AttachmentKind::File,
                markers::MarkerKind::Voice => AttachmentKind::Voice,
            };
            let resolved = match validate_marker_target(&marker.target, outbox.workspace_dir) {
                Ok(t) => t,
                Err(e) => {
                    let kind = e.as_marker_failure();
                    let label = match kind {
                        MarkerFailure::Refused => "trust boundary",
                        MarkerFailure::Failed => "not found",
                    };
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        &format!(
                            "matrix: skipping outbound marker for {} ({label}): {e}",
                            marker.target
                        )
                    );
                    failed_markers.push((marker.target.clone(), kind));
                    continue;
                }
            };
            let bytes = match resolved {
                MarkerTarget::Local(path) => match tokio::fs::read(&path).await {
                    Ok(b) => b,
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                            &format!(
                                "matrix: skipping outbound marker for {} (read failed): {e}",
                                marker.target
                            )
                        );
                        failed_markers.push((marker.target.clone(), MarkerFailure::Failed));
                        continue;
                    }
                },
                MarkerTarget::Http(url) => match fetch_http(url).await {
                    Ok(b) => b,
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                            &format!(
                                "matrix: skipping outbound marker for {} (http failed): {e}",
                                marker.target
                            )
                        );
                        failed_markers.push((marker.target.clone(), MarkerFailure::Failed));
                        continue;
                    }
                },
            };
            let file_name = derive_file_name(&marker.target);
            let mime = mime_for(&file_name, &kind);
            let att = MediaAttachment {
                file_name,
                data: bytes,
                mime_type: Some(mime),
            };
            match upload_attachment(room, &att, kind, thread_anchor).await {
                Ok(id) => last_attachment_id = Some(id),
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        &format!(
                            "matrix: skipping outbound marker for {} (upload failed): {e}",
                            marker.target
                        )
                    );
                    failed_markers.push((marker.target.clone(), MarkerFailure::Failed));
                }
            }
        }

        if !failed_markers.is_empty() {
            let targets: Vec<&str> = failed_markers.iter().map(|(t, _)| t.as_str()).collect();
            let note = if targets.len() == 1 {
                format!("(note: I couldn't deliver the file at {}.)", targets[0])
            } else {
                let joined = targets.join(", ");
                format!("(note: I couldn't deliver these files: {joined}.)")
            };
            text = if text.trim().is_empty() {
                note
            } else {
                format!("{text}\n\n{note}")
            };
        }

        Ok(AttachmentDelivery {
            text,
            last_attachment_id,
            failed_markers,
        })
    }

    pub(super) async fn send(outbox: &Outbox<'_>, message: &SendMessage) -> Result<OwnedEventId> {
        let room =
            resolve_joined_room(outbox.client, outbox.alias_cache, &message.recipient).await?;

        let (text, ms) = markers::parse(&message.content);

        // Build the thread anchor used by both attachment uploads and the
        // text reply, so attachments live in the same thread instead of
        // landing in the main timeline.
        let thread_anchor = thread_anchor_from_message(outbox, message);

        let delivery = deliver_attachments(
            outbox,
            &room,
            text,
            &ms,
            &message.attachments,
            thread_anchor.as_ref(),
        )
        .await?;

        // Decide whether to send the text, return the last attachment's
        // event_id, or surface an error. Marker-only messages used to error
        // here even though their attachment had landed; the runtime would
        // see Err and could retry, producing duplicate uploads.
        match decide_send_outcome(
            delivery.text.trim().is_empty(),
            delivery.last_attachment_id.is_some(),
        ) {
            SendOutcome::SendText => {}
            SendOutcome::ReturnAttachment => {
                // Safe by construction: ReturnAttachment is only returned
                // when last_attachment_id is Some.
                let kinds = delivery.failure_kinds();
                let attachment_id = delivery
                    .last_attachment_id
                    .expect("decide_send_outcome guarantees Some when ReturnAttachment");
                emit_failure_reactions(&room, &attachment_id, &kinds).await;
                return Ok(attachment_id);
            }
            SendOutcome::EmptyError => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"phase": "send"})),
                    "matrix: empty message body and no successful attachment"
                );
                return Err(anyhow::Error::msg(
                    "matrix: empty message body and no successful attachment",
                ));
            }
        }

        let content = RoomMessageEventContent::text_markdown(&delivery.text);

        let event_id = if let (true, Some(anchor)) = (
            outbox.reply_in_thread,
            message.thread_ts.as_deref().filter(|s| !s.is_empty()),
        ) {
            send_threaded_reply(&room, content, anchor, outbox.threads_seen).await?
        } else {
            room.send(content).await?.response.event_id
        };

        let kinds = delivery.failure_kinds();
        emit_failure_reactions(&room, &event_id, &kinds).await;

        Ok(event_id)
    }

    /// Best-effort: apply 🚫 / ⚠️ reactions to the bot's just-sent message
    /// based on which kinds of marker failures occurred. Reaction send
    /// failures are logged but never propagated — the primary message
    /// already landed.
    pub(super) async fn emit_failure_reactions(
        room: &Room,
        event_id: &OwnedEventId,
        failures: &[MarkerFailure],
    ) {
        for emoji in decide_reactions(failures) {
            let content =
                ReactionEventContent::new(Annotation::new(event_id.clone(), emoji.to_string()));
            if let Err(e) = room.send(content).await {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"error": format!("{}", e), "emoji": emoji})
                        ),
                    "matrix: failed to send reaction on outgoing message"
                );
            }
        }
    }

    async fn send_threaded_reply(
        room: &Room,
        content: RoomMessageEventContent,
        anchor_id: &str,
        threads_seen: &Arc<TokioRwLock<std::collections::HashSet<OwnedEventId>>>,
    ) -> Result<OwnedEventId> {
        let anchor: OwnedEventId = anchor_id
            .parse()
            .with_context(|| format!("parse thread anchor {anchor_id}"))?;
        let without_relation = RoomMessageEventContentWithoutRelation::new(content.msgtype.clone());
        let reply_event = room
            .make_reply_event(
                without_relation,
                Reply {
                    event_id: anchor.clone(),
                    enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
                    add_mentions: AddMentions::No,
                },
            )
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "make_reply_event failed"
                );
                anyhow::Error::msg(format!("make_reply_event failed: {e}"))
            })?;
        ctx_mod::mark_seen(threads_seen, anchor).await;
        let resp = room.send(reply_event).await?;
        Ok(resp.response.event_id)
    }

    pub(super) async fn edit(
        client: &Client,
        room_id: &str,
        event_id: &OwnedEventId,
        text: &str,
    ) -> Result<()> {
        let room = client
            .get_room(&room_id.parse::<OwnedRoomId>()?)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"room_id": room_id})),
                    "matrix: room not joined"
                );
                anyhow::Error::msg(format!("matrix: room not joined: {room_id}"))
            })?;
        let new_content = RoomMessageEventContentWithoutRelation::new(MessageType::Text(
            TextMessageEventContent::markdown(text),
        ));
        let edit_event = room
            .make_edit_event(event_id, EditedContent::RoomMessage(new_content))
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "make_edit_event failed"
                );
                anyhow::Error::msg(format!("make_edit_event failed: {e}"))
            })?;
        room.send(edit_event).await?;
        Ok(())
    }

    pub(super) async fn redact(
        client: &Client,
        room_id: &str,
        event_id: &OwnedEventId,
        reason: Option<String>,
    ) -> Result<()> {
        let room = client
            .get_room(&room_id.parse::<OwnedRoomId>()?)
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"room_id": room_id})),
                    "matrix: room not joined"
                );
                anyhow::Error::msg(format!("matrix: room not joined: {room_id}"))
            })?;
        room.redact(event_id, reason.as_deref(), None).await?;
        Ok(())
    }

    pub(super) async fn react(
        outbox: &Outbox<'_>,
        room_id: &str,
        event_id: &OwnedEventId,
        emoji: &str,
    ) -> Result<()> {
        let room = resolve_joined_room(outbox.client, outbox.alias_cache, room_id).await?;
        let content =
            ReactionEventContent::new(Annotation::new(event_id.clone(), emoji.to_string()));
        let resp = room.send(content).await?;
        outbox.reaction_log.lock().await.insert(
            (
                room.room_id().to_owned(),
                event_id.clone(),
                emoji.to_string(),
            ),
            resp.response.event_id,
        );
        Ok(())
    }

    pub(super) async fn unreact(
        outbox: &Outbox<'_>,
        room_id: &str,
        event_id: &OwnedEventId,
        emoji: &str,
    ) -> Result<()> {
        let room = resolve_joined_room(outbox.client, outbox.alias_cache, room_id).await?;
        let key = (
            room.room_id().to_owned(),
            event_id.clone(),
            emoji.to_string(),
        );
        let reaction_event_id = outbox.reaction_log.lock().await.remove(&key);
        if let Some(rid) = reaction_event_id {
            room.redact(&rid, Some("removing reaction"), None).await?;
        }
        Ok(())
    }

    pub(super) async fn resolve_joined_room(
        client: &Client,
        cache: &Arc<TokioRwLock<HashMap<String, OwnedRoomId>>>,
        recipient: &str,
    ) -> Result<Room> {
        let id = client::resolve_room(client, cache, recipient).await?;
        let room = client.get_room(&id).ok_or_else(|| {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"recipient": recipient})),
                "matrix: bot is not in room"
            );
            anyhow::Error::msg(format!("matrix: bot is not in room {recipient}"))
        })?;
        if room.state() != RoomState::Joined {
            bail!("matrix: room {recipient} is not in joined state");
        }
        Ok(room)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum AttachmentKind {
        Auto,
        Image,
        Audio,
        Video,
        File,
        Voice,
    }

    async fn upload_attachment(
        room: &Room,
        att: &MediaAttachment,
        kind: AttachmentKind,
        thread_anchor: Option<&OwnedEventId>,
    ) -> Result<OwnedEventId> {
        let mime = attachment_mime(att);
        if matches!(kind, AttachmentKind::Voice) {
            return upload_voice(room, att, &mime, thread_anchor).await;
        }
        let config = attachment_config_for(att, kind, &mime, thread_anchor);
        let resp = room
            .send_attachment(att.file_name.clone(), &mime, att.data.clone(), config)
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "send_attachment failed"
                );
                anyhow::Error::msg(format!("send_attachment failed: {e}"))
            })?;
        Ok(resp.event_id)
    }

    pub(super) fn attachment_config_for(
        att: &MediaAttachment,
        kind: AttachmentKind,
        mime: &mime_guess::Mime,
        thread_anchor: Option<&OwnedEventId>,
    ) -> AttachmentConfig {
        let mut config = AttachmentConfig::new().info(attachment_info_for(att, kind, mime));
        if let Some(anchor) = thread_anchor {
            config = config.reply(Some(Reply {
                event_id: anchor.clone(),
                enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
                add_mentions: AddMentions::No,
            }));
        }
        config
    }

    pub(super) fn attachment_mime(att: &MediaAttachment) -> mime_guess::Mime {
        match att.mime_type.as_deref() {
            Some(m) => m
                .parse()
                .unwrap_or(mime_guess::mime::APPLICATION_OCTET_STREAM),
            None => mime_guess::from_path(&att.file_name)
                .first()
                .unwrap_or(mime_guess::mime::APPLICATION_OCTET_STREAM),
        }
    }

    fn attachment_info_for(
        att: &MediaAttachment,
        kind: AttachmentKind,
        mime: &mime_guess::Mime,
    ) -> AttachmentInfo {
        let size = UInt::try_from(att.data.len()).ok();
        match attachment_info_kind(kind, mime) {
            AttachmentKind::Image => AttachmentInfo::Image(BaseImageInfo {
                size,
                ..Default::default()
            }),
            AttachmentKind::Audio => AttachmentInfo::Audio(BaseAudioInfo {
                size,
                ..Default::default()
            }),
            AttachmentKind::Video => AttachmentInfo::Video(BaseVideoInfo {
                size,
                ..Default::default()
            }),
            AttachmentKind::Voice => AttachmentInfo::Voice(BaseAudioInfo {
                size,
                ..Default::default()
            }),
            AttachmentKind::File | AttachmentKind::Auto => {
                AttachmentInfo::File(BaseFileInfo { size })
            }
        }
    }

    fn attachment_info_kind(kind: AttachmentKind, mime: &mime_guess::Mime) -> AttachmentKind {
        if kind == AttachmentKind::Voice {
            return AttachmentKind::Voice;
        }
        match mime.type_() {
            mime_guess::mime::IMAGE => AttachmentKind::Image,
            mime_guess::mime::AUDIO => AttachmentKind::Audio,
            mime_guess::mime::VIDEO => AttachmentKind::Video,
            _ => AttachmentKind::File,
        }
    }

    /// Voice messages need the `org.matrix.msc3245.voice` flag, which the
    /// stable matrix-sdk types don't carry. Send via raw JSON, attaching the
    /// thread relation manually when the bot is replying inside one.
    async fn upload_voice(
        room: &Room,
        att: &MediaAttachment,
        mime: &mime_guess::Mime,
        thread_anchor: Option<&OwnedEventId>,
    ) -> Result<OwnedEventId> {
        let mxc = room
            .client()
            .media()
            .upload(mime, att.data.clone(), None)
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "media upload failed"
                );
                anyhow::Error::msg(format!("media upload failed: {e}"))
            })?;
        let mut event = json!({
            "msgtype": "m.audio",
            "body": att.file_name,
            "filename": att.file_name,
            "url": mxc.content_uri.to_string(),
            "info": {
                "mimetype": mime.essence_str(),
                "size": att.data.len(),
            },
            "org.matrix.msc3245.voice": {},
            "org.matrix.msc1767.audio": {
                "duration": 0u32,
                "waveform": Vec::<u32>::new(),
            },
        });
        if let Some(anchor) = thread_anchor
            && let Some(obj) = event.as_object_mut()
        {
            obj.insert(
                "m.relates_to".to_string(),
                json!({
                    "rel_type": "m.thread",
                    "event_id": anchor.as_str(),
                    "is_falling_back": true,
                    "m.in_reply_to": { "event_id": anchor.as_str() },
                }),
            );
        }
        let resp = room.send_raw("m.room.message", event).await?;
        Ok(resp.response.event_id)
    }

    fn derive_file_name(target: &str) -> String {
        target
            .rsplit_once('/')
            .map(|(_, n)| n.to_string())
            .unwrap_or_else(|| target.to_string())
    }

    fn mime_for(file_name: &str, kind: &AttachmentKind) -> String {
        if let Some(m) = mime_guess::from_path(file_name).first() {
            return m.essence_str().to_string();
        }
        match kind {
            AttachmentKind::Image => "image/jpeg".to_string(),
            AttachmentKind::Audio | AttachmentKind::Voice => "audio/ogg".to_string(),
            AttachmentKind::Video => "video/mp4".to_string(),
            AttachmentKind::File | AttachmentKind::Auto => "application/octet-stream".to_string(),
        }
    }
}

// ─── public type ───────────────────────────────────────────────────────────

/// Matrix channel.
pub struct MatrixChannel {
    config: Arc<MatrixConfig>,
    /// The alias key under `[channels.matrix.<alias>]` this handle is
    /// bound to. Used to scope peer-group writes and resolver lookups.
    alias: String,
    /// Resolves inbound external peers from canonical state at message-time.
    /// No cache (see AGENTS.md "ABSOLUTE RULE — SINGLE SOURCE OF TRUTH").
    peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    state_dir: PathBuf,
    workspace_dir: Option<Arc<PathBuf>>,
    transcription: Option<Arc<TranscriptionConfig>>,
    client: tokio::sync::OnceCell<Client>,
    pending_approvals: Arc<TokioMutex<HashMap<String, oneshot::Sender<ChannelApprovalResponse>>>>,
    streaming_state: Arc<TokioRwLock<streaming::State>>,
    threads_seen: Arc<TokioRwLock<HashSet<OwnedEventId>>>,
    alias_cache: Arc<TokioRwLock<HashMap<String, OwnedRoomId>>>,
    reaction_log: Arc<TokioMutex<HashMap<outbound::ReactionKey, OwnedEventId>>>,
    bot_display_name: Arc<TokioRwLock<Option<String>>>,
    initial_sync_done: Arc<AtomicBool>,
    undecryptable_seen: Arc<TokioMutex<HashSet<OwnedEventId>>>,
    /// Resolved `ack_reactions` for this Matrix instance — the
    /// per-channel `MatrixConfig.ack_reactions` override falls back to
    /// `[channels].ack_reactions` here at construction time, so the
    /// read site doesn't need to re-resolve on every reaction.
    ack_reactions: bool,
}

impl MatrixChannel {
    /// Validate config and prepare the channel. The SDK Client is built lazily
    /// on first `listen()` or `send()` call.
    pub fn new(
        config: MatrixConfig,
        alias: impl Into<String>,
        peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        state_dir: PathBuf,
    ) -> Result<Self> {
        if config.homeserver.trim().is_empty() {
            bail!("matrix: `homeserver` is required");
        }
        let has_token = config
            .access_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
        let has_password = config
            .password
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());
        if !has_token && !has_password {
            bail!("matrix: configure either `access_token` or `password`");
        }
        let ack_reactions = config.ack_reactions.unwrap_or(true);
        Ok(Self {
            config: Arc::new(config),
            alias: alias.into(),
            peer_resolver,
            state_dir,
            workspace_dir: None,
            transcription: None,
            client: tokio::sync::OnceCell::new(),
            pending_approvals: Arc::new(TokioMutex::new(HashMap::new())),
            streaming_state: Arc::new(TokioRwLock::new(streaming::State::default())),
            threads_seen: Arc::new(TokioRwLock::new(HashSet::new())),
            alias_cache: Arc::new(TokioRwLock::new(HashMap::new())),
            reaction_log: Arc::new(TokioMutex::new(HashMap::new())),
            bot_display_name: Arc::new(TokioRwLock::new(None)),
            initial_sync_done: Arc::new(AtomicBool::new(false)),
            undecryptable_seen: Arc::new(TokioMutex::new(HashSet::new())),
            ack_reactions,
        })
    }

    /// Return the alias under `[channels.matrix.<alias>]` that this
    /// channel handle is bound to.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    #[must_use]
    pub fn with_ack_reactions(mut self, ack_reactions: bool) -> Self {
        self.ack_reactions = ack_reactions;
        self
    }

    pub fn with_transcription(mut self, transcription: TranscriptionConfig) -> Self {
        self.transcription = Some(Arc::new(transcription));
        self
    }

    /// Configure the workspace directory used to persist downloaded media so
    /// the agent's vision/document pipelines can read inbound files via
    /// `[IMAGE:path]` / `[Document: name] path` markers.
    pub fn with_workspace_dir(mut self, dir: PathBuf) -> Self {
        self.workspace_dir = Some(Arc::new(dir));
        self
    }

    async fn ensure_client(&self) -> Result<&Client> {
        use ::zeroclaw_log::__private::tracing::Instrument;
        self.client
            .get_or_try_init(|| {
                async {
                    let c = client::build(&self.config, &self.state_dir).await?;
                    if let Ok(Some(name)) = c.account().get_display_name().await {
                        *self.bot_display_name.write().await = Some(name);
                    }
                    Ok::<_, anyhow::Error>(c)
                }
                .instrument(::zeroclaw_log::attribution_span!(self))
            })
            .await
    }

    fn outbox<'a>(&'a self, client: &'a Client) -> outbound::Outbox<'a> {
        outbound::Outbox {
            client,
            alias_cache: &self.alias_cache,
            threads_seen: &self.threads_seen,
            reaction_log: &self.reaction_log,
            reply_in_thread: self.config.reply_in_thread,
            workspace_dir: self.workspace_dir.as_deref().map(|p| p.as_path()),
        }
    }

    /// Edit-in-place draft update. Rate-limited per the configured interval.
    async fn partial_update(&self, recipient: &str, message_id: &str, text: &str) -> Result<()> {
        let client = self.ensure_client().await?;
        let key = streaming_key(recipient, message_id)?;
        let Some(visible_text) = streaming::partial_visible_text(text) else {
            return Ok(());
        };
        let event_id = {
            let mut state = self.streaming_state.write().await;
            let Some(draft) = streaming::partial_for_update(&mut state, &key) else {
                return Ok(());
            };
            let now = Instant::now();
            let interval = Duration::from_millis(self.config.draft_update_interval_ms.max(50));
            if !streaming::partial_should_edit(draft, &visible_text, now, interval) {
                return Ok(());
            }
            let event_id = draft.event_id.clone();
            draft.last_text = visible_text.clone();
            draft.last_edit = now;
            event_id
        };
        outbound::edit(client, recipient, &event_id, &visible_text).await
    }

    /// MultiMessage paragraph emitter. Loops emitting one paragraph per
    /// `\n\n` boundary until the unsent buffer no longer contains a break,
    /// then returns to wait for more accumulated text. Each paragraph posts
    /// as an independent room message threaded under the captured anchor.
    async fn multi_update(&self, recipient: &str, message_id: &str, text: &str) -> Result<()> {
        let client = self.ensure_client().await?;
        let key = streaming_key(recipient, message_id)?;
        let delay = Duration::from_millis(self.config.multi_message_delay_ms);
        loop {
            let (paragraph, thread_anchor) = {
                let mut state = self.streaming_state.write().await;
                let Some(multi) = streaming::multi_for_update(&mut state, &key) else {
                    return Ok(());
                };
                // Detect a buffer reset (e.g. DraftEvent::Clear) and re-anchor
                // to the new shorter text.
                if text.len() < multi.sent_so_far {
                    multi.sent_so_far = 0;
                    return Ok(());
                }
                if text.len() == multi.sent_so_far {
                    return Ok(());
                }
                let unsent = &text[multi.sent_so_far..];
                let Some(break_at) = streaming::next_paragraph_break(unsent) else {
                    return Ok(());
                };
                let paragraph = unsent[..break_at].trim().to_string();
                multi.sent_so_far += break_at + 2; // +2 for the consumed "\n\n"
                (paragraph, multi.thread_anchor.clone())
            };
            if !paragraph.is_empty() {
                let mut msg = SendMessage::new(paragraph, recipient);
                msg.thread_ts = thread_anchor.as_ref().map(|e| e.to_string());
                if let Err(e) = outbound::send(&self.outbox(client), &msg).await {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "matrix: multi-message paragraph send failed"
                    );
                }
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

impl ::zeroclaw_api::attribution::Attributable for MatrixChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(::zeroclaw_api::attribution::ChannelKind::Matrix)
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        "matrix"
    }

    fn self_handle(&self) -> Option<String> {
        self.client
            .get()
            .and_then(|c| c.user_id().map(|u| u.to_string()))
    }

    fn self_addressed_mention(&self) -> Option<String> {
        self.self_handle()
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let client = self.ensure_client().await?;
        let _ = outbound::send(&self.outbox(client), message).await?;
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        let client = self.ensure_client().await?.clone();
        let user_id = client
            .user_id()
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "matrix: client has no user_id after login"
                );
                anyhow::Error::msg("matrix: client has no user_id after login")
            })?
            .to_owned();
        let ctx = inbound::HandlerCtx {
            config: self.config.clone(),
            alias: self.alias.clone(),
            peer_resolver: self.peer_resolver.clone(),
            transcription: self.transcription.clone(),
            workspace_dir: self.workspace_dir.clone(),
            tx,
            pending_approvals: self.pending_approvals.clone(),
            threads_seen: self.threads_seen.clone(),
            bot_user_id: user_id,
            bot_display_name: self.bot_display_name.clone(),
            initial_sync_done: self.initial_sync_done.clone(),
            undecryptable_seen: self.undecryptable_seen.clone(),
        };
        inbound::run_sync_loop(client, ctx).await
    }

    async fn health_check(&self) -> bool {
        match self.client.get() {
            Some(c) => c.matrix_auth().logged_in() && self.initial_sync_done.load(Ordering::SeqCst),
            None => false,
        }
    }

    async fn start_typing(&self, recipient: &str) -> Result<()> {
        let client = self.ensure_client().await?;
        let id = client::resolve_room(client, &self.alias_cache, recipient).await?;
        if let Some(room) = client.get_room(&id) {
            let _ = room.typing_notice(true).await;
        }
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> Result<()> {
        let client = self.ensure_client().await?;
        let id = client::resolve_room(client, &self.alias_cache, recipient).await?;
        if let Some(room) = client.get_room(&id) {
            let _ = room.typing_notice(false).await;
        }
        Ok(())
    }

    fn supports_draft_updates(&self) -> bool {
        // The orchestrator's streaming pipeline is gated on this returning
        // true. Both Partial and MultiMessage need it on so update_draft is
        // driven with accumulated text; the channel decides internally
        // whether to edit a single message or emit paragraphs.
        !matches!(self.config.stream_mode, StreamMode::Off)
    }

    fn supports_multi_message_streaming(&self) -> bool {
        matches!(self.config.stream_mode, StreamMode::MultiMessage)
    }

    fn multi_message_delay_ms(&self) -> u64 {
        self.config.multi_message_delay_ms
    }

    async fn send_draft(&self, message: &SendMessage) -> Result<Option<String>> {
        let client = self.ensure_client().await?;
        let room_id = streaming_room(&message.recipient)?;
        match self.config.stream_mode {
            StreamMode::Off => Ok(None),
            StreamMode::Partial => {
                // Send the placeholder draft now so subsequent update_draft
                // calls have an event to edit.
                let event_id = outbound::send(&self.outbox(client), message).await?;
                let thread_anchor =
                    outbound::thread_anchor_from_message(&self.outbox(client), message);
                let key = streaming::draft_key(room_id, event_id.as_ref())?;
                let mut state = self.streaming_state.write().await;
                state.partial.insert(
                    key,
                    streaming::PartialDraft {
                        event_id: event_id.clone(),
                        thread_anchor,
                        last_text: message.content.clone(),
                        last_edit: Instant::now(),
                    },
                );
                Ok(Some(event_id.to_string()))
            }
            StreamMode::MultiMessage => {
                // No initial message — paragraphs are emitted by update_draft
                // as they appear. Capture the thread anchor up front so each
                // paragraph lands in the same thread as the user's message.
                let thread_anchor = message
                    .thread_ts
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .and_then(|s| s.parse::<OwnedEventId>().ok());
                let draft_id = streaming::new_multi_message_draft_id();
                let key = streaming::draft_key(room_id, &draft_id)?;
                let mut state = self.streaming_state.write().await;
                state.multi.insert(
                    key,
                    streaming::MultiDraft {
                        thread_anchor,
                        sent_so_far: 0,
                    },
                );
                Ok(Some(draft_id))
            }
        }
    }

    async fn update_draft(&self, recipient: &str, message_id: &str, text: &str) -> Result<()> {
        match self.config.stream_mode {
            StreamMode::Off => Ok(()),
            StreamMode::Partial => self.partial_update(recipient, message_id, text).await,
            StreamMode::MultiMessage => self.multi_update(recipient, message_id, text).await,
        }
    }

    async fn update_draft_progress(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> Result<()> {
        // Tool-status updates only show in Partial (edit-in-place) mode.
        // MultiMessage doesn't have an in-flight draft to update.
        if matches!(self.config.stream_mode, StreamMode::Partial) {
            return self.update_draft(recipient, message_id, text).await;
        }
        Ok(())
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
        _suppress_voice: bool,
    ) -> Result<()> {
        let client = self.ensure_client().await?;
        let key = streaming_key(recipient, message_id)?;
        match self.config.stream_mode {
            StreamMode::Off => Ok(()),
            StreamMode::Partial => {
                let draft = {
                    let mut state = self.streaming_state.write().await;
                    streaming::take_partial(&mut state, &key)
                };
                if let Some(draft) = draft {
                    let room =
                        outbound::resolve_joined_room(client, &self.alias_cache, recipient).await?;
                    let (cleaned_text, markers) = markers::parse(text);
                    let delivery = outbound::deliver_attachments(
                        &self.outbox(client),
                        &room,
                        cleaned_text,
                        &markers,
                        &[],
                        draft.thread_anchor.as_ref(),
                    )
                    .await?;

                    match streaming::decide_partial_finalize_action(
                        delivery.text.trim().is_empty(),
                        delivery.last_attachment_id.is_some(),
                    ) {
                        streaming::PartialFinalizeAction::EditDraft => {
                            let kinds = delivery.failure_kinds();
                            let any_attachment_landed = delivery.last_attachment_id.is_some();
                            if let Err(edit_err) =
                                outbound::edit(client, recipient, &draft.event_id, &delivery.text)
                                    .await
                            {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        ::serde_json::json!({"edit_err": edit_err.to_string()})
                                    ),
                                    "matrix: partial finalize edit failed: ; sending cleaned text fallback"
                                );
                                let mut fallback = SendMessage::new(&delivery.text, recipient);
                                fallback.thread_ts =
                                    draft.thread_anchor.as_ref().map(|e| e.to_string());
                                match outbound::send(&self.outbox(client), &fallback).await {
                                    Ok(fallback_id) => {
                                        outbound::emit_failure_reactions(
                                            &room,
                                            &fallback_id,
                                            &kinds,
                                        )
                                        .await;
                                    }
                                    Err(send_err) if any_attachment_landed => {
                                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"send_err": send_err.to_string()})), "matrix: partial finalize cleaned text fallback failed after attachment upload: ; suppressing error to avoid duplicate attachment retry");
                                    }
                                    Err(send_err) => {
                                        return Err(edit_err).with_context(|| {
                                            format!(
                                                "matrix: partial finalize cleaned text fallback failed: {send_err}"
                                            )
                                        });
                                    }
                                }
                            } else {
                                outbound::emit_failure_reactions(&room, &draft.event_id, &kinds)
                                    .await;
                            }
                        }
                        streaming::PartialFinalizeAction::RedactDraft => {
                            if let Err(err) = outbound::redact(
                                client,
                                recipient,
                                &draft.event_id,
                                Some("attachment-only response delivered".to_string()),
                            )
                            .await
                            {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(::serde_json::json!({"err": err.to_string()})),
                                    "matrix: partial finalize redaction failed after attachment-only upload: ; leaving placeholder to avoid duplicate attachment retry"
                                );
                            }
                        }
                        streaming::PartialFinalizeAction::EmptyError => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Reject
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"phase": "partial_finalize"})),
                                "matrix: empty partial draft body and no successful attachment"
                            );
                            return Err(anyhow::Error::msg(
                                "matrix: empty partial draft body and no successful attachment",
                            ));
                        }
                    }
                }
                Ok(())
            }
            StreamMode::MultiMessage => {
                // Drain the trailing paragraph (or whatever's left after the
                // last \n\n boundary) as one final message.
                let multi = {
                    let mut state = self.streaming_state.write().await;
                    streaming::take_multi(&mut state, &key)
                };
                let Some(state) = multi else {
                    return Ok(());
                };
                let remainder = if text.len() > state.sent_so_far {
                    text[state.sent_so_far..].trim().to_string()
                } else {
                    String::new()
                };
                if !remainder.is_empty() {
                    let mut msg = SendMessage::new(remainder, recipient);
                    msg.thread_ts = state.thread_anchor.as_ref().map(|e| e.to_string());
                    outbound::send(&self.outbox(client), &msg).await?;
                }
                Ok(())
            }
        }
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> Result<()> {
        let client = self.ensure_client().await?;
        let key = streaming_key(recipient, message_id)?;
        match self.config.stream_mode {
            StreamMode::Off => Ok(()),
            StreamMode::Partial => {
                let draft = {
                    let mut state = self.streaming_state.write().await;
                    streaming::take_partial(&mut state, &key)
                };
                if let Some(d) = draft {
                    let _ = outbound::redact(
                        client,
                        recipient,
                        &d.event_id,
                        Some("cancelled".to_string()),
                    )
                    .await;
                }
                Ok(())
            }
            StreamMode::MultiMessage => {
                // Already-sent paragraphs are independent room messages and
                // are not redacted on cancel — partial output is preferable
                // to silent disappearance. Just drop our state.
                let mut state = self.streaming_state.write().await;
                streaming::take_multi(&mut state, &key);
                Ok(())
            }
        }
    }

    async fn add_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        if !self.ack_reactions {
            return Ok(());
        }
        let client = self.ensure_client().await?;
        let event_id: OwnedEventId = message_id.parse()?;
        outbound::react(&self.outbox(client), channel_id, &event_id, emoji).await
    }

    async fn remove_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        if !self.ack_reactions {
            return Ok(());
        }
        let client = self.ensure_client().await?;
        let event_id: OwnedEventId = message_id.parse()?;
        outbound::unreact(&self.outbox(client), channel_id, &event_id, emoji).await
    }

    async fn redact_message(
        &self,
        channel_id: &str,
        message_id: &str,
        reason: Option<String>,
    ) -> Result<()> {
        let client = self.ensure_client().await?;
        let event_id: OwnedEventId = message_id.parse()?;
        outbound::redact(client, channel_id, &event_id, reason).await
    }

    async fn create_room(&self, options: &RoomCreationOptions) -> Result<String> {
        let client = self.ensure_client().await?;
        let request = room_management::build_create_room_request(options)?;
        let room = client.create_room(request).await?;
        Ok(room.room_id().to_string())
    }

    async fn invite_user(&self, room_id: &str, user_id: &str) -> Result<()> {
        let client = self.ensure_client().await?;
        let request = room_management::build_invite_user_request(room_id, user_id)?;
        client.send(request).await?;
        Ok(())
    }

    /// Delegates to [`Self::request_approval_attributed`] and drops the
    /// provenance, so the prompt/timeout logic lives in exactly one place.
    async fn request_approval(
        &self,
        recipient: &str,
        request: &ChannelApprovalRequest,
    ) -> Result<Option<ChannelApprovalResponse>> {
        Ok(self
            .request_approval_attributed(recipient, request)
            .await?
            .map(|attributed| attributed.response))
    }

    async fn request_approval_attributed(
        &self,
        recipient: &str,
        request: &ChannelApprovalRequest,
    ) -> Result<Option<zeroclaw_api::channel::AttributedApprovalResponse>> {
        let token = approval::generate_token_default();
        let prompt = format!(
            "APPROVAL REQUIRED [{token}]\nTool: {}\nArgs: {}\n\nReply `{token} approve` / `{token} deny` / `{token} always`.",
            request.tool_name, request.arguments_summary
        );

        let (tx, rx) = oneshot::channel();
        self.pending_approvals
            .lock()
            .await
            .insert(token.clone(), tx);

        let send_msg = SendMessage::new(prompt, recipient);
        if let Err(e) = self.send(&send_msg).await {
            self.pending_approvals.lock().await.remove(&token);
            return Err(e);
        }

        let timeout = Duration::from_secs(self.config.approval_timeout_secs.max(1));
        let result = tokio::time::timeout(timeout, rx).await;
        if result.is_err() {
            self.pending_approvals.lock().await.remove(&token);
        }
        // Only the first arm is an operator decision; the other two are the
        // runtime denying because nobody replied, and must say so.
        match result {
            Ok(Ok(resp)) => Ok(Some(
                zeroclaw_api::channel::AttributedApprovalResponse::operator(resp),
            )),
            Ok(Err(_)) => Ok(Some(
                zeroclaw_api::channel::AttributedApprovalResponse::from_runtime(
                    ChannelApprovalResponse::Deny,
                    zeroclaw_api::channel::ApprovalSource::Unreachable,
                ),
            )),
            Err(_) => Ok(Some(
                zeroclaw_api::channel::AttributedApprovalResponse::from_runtime(
                    ChannelApprovalResponse::Deny,
                    zeroclaw_api::channel::ApprovalSource::TimedOut,
                ),
            )),
        }
    }
}

fn streaming_room(recipient: &str) -> Result<OwnedRoomId> {
    recipient
        .parse::<OwnedRoomId>()
        .with_context(|| format!("parse recipient room id {recipient}"))
}

fn streaming_key(recipient: &str, message_id: &str) -> Result<streaming::DraftKey> {
    streaming::draft_key(streaming_room(recipient)?, message_id)
}

// ─── tests ─────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests;
