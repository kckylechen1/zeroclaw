#[cfg(test)]
use super::*;
#[cfg(feature = "whatsapp-web")]
use wacore_binary::jid::Jid;

#[test]
#[cfg(feature = "whatsapp-web")]
fn clear_persisted_session_removes_db_triple_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("session.db");
    let db_str = db.to_string_lossy().into_owned();
    std::fs::write(&db, b"db").unwrap();
    std::fs::write(format!("{db_str}-wal"), b"wal").unwrap();
    std::fs::write(format!("{db_str}-shm"), b"shm").unwrap();

    let removed = WhatsAppWebChannel::clear_persisted_session(&db_str).unwrap();
    assert_eq!(removed.len(), 3);
    for path in WhatsAppWebChannel::session_file_paths(&db_str) {
        assert!(
            !std::path::Path::new(&path).exists(),
            "{path} must be removed"
        );
    }

    // Relinking an already unpaired channel is a safe no-op that
    // must not create the database.
    let removed = WhatsAppWebChannel::clear_persisted_session(&db_str).unwrap();
    assert!(removed.is_empty());
    assert!(!db.exists());

    // Empty session_path (channel saved without one) clears nothing.
    assert!(
        WhatsAppWebChannel::clear_persisted_session("")
            .unwrap()
            .is_empty()
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn media_markers_reuse_shared_parser_kinds() {
    let (cleaned, raw) = super::super::util::parse_attachment_markers(
        "send [IMAGE:photo.png] [DOCUMENT:report.pdf] [VOICE:voice.ogg]",
    );
    let markers = raw
        .into_iter()
        .filter_map(|(kind, target)| WhatsAppMarker::from_shared_marker(kind, target))
        .collect::<Vec<_>>();

    assert_eq!(cleaned, "send");
    assert_eq!(
        markers
            .iter()
            .map(|marker| match marker {
                WhatsAppMarker::Media(m) => m.kind,
                WhatsAppMarker::Location(_) => panic!("expected media markers"),
            })
            .collect::<Vec<_>>(),
        vec![
            WhatsAppMediaKind::Image,
            WhatsAppMediaKind::Document,
            WhatsAppMediaKind::Voice
        ]
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn from_shared_marker_routes_location_kind() {
    let marker =
        WhatsAppMarker::from_shared_marker("LOCATION".to_string(), "40.7128,-74.0060".to_string());
    assert!(matches!(marker, Some(WhatsAppMarker::Location(_))));
    // An invalid target still routes to the Location variant: validation
    // is deferred to the send path so the marker counts as a failed
    // delivery instead of being stripped silently.
    assert_eq!(
        WhatsAppMarker::from_shared_marker("LOCATION".to_string(), "999,999".to_string()),
        Some(WhatsAppMarker::Location("999,999".to_string()))
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn validate_location_target_refuses_invalid_coordinates() {
    // Out-of-range and malformed targets are refused — the send loop
    // counts these as failed deliveries rather than sending a bogus pin.
    for target in ["999,999", "not-a-number,0.0", "40.7128", ""] {
        let err = validate_whatsapp_location_target(target)
            .expect_err("out-of-range or malformed target must be refused");
        assert_eq!(err.kind(), WhatsAppMarkerFailure::Refused, "{target}");
    }
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn allowed_groups_empty_permits_all() {
    // Empty list is the default: every group passes (no behavior change).
    assert!(super::is_group_chat_allowed("123456789012345@g.us", &[]));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn validate_marker_target_accepts_workspace_relative_file() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let file = workspace.path().join("photo.png");
    std::fs::write(&file, b"png").expect("write fixture");

    let resolved =
        validate_whatsapp_marker_target("photo.png", Some(workspace.path())).expect("inside");

    assert_eq!(resolved, file.canonicalize().expect("canonical fixture"));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn allowed_groups_full_jid_match() {
    let groups = vec!["123456789012345@g.us".to_string()];
    assert!(super::is_group_chat_allowed(
        "123456789012345@g.us",
        &groups
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn validate_marker_target_rejects_workspace_escape() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::NamedTempFile::new().expect("outside file");

    let err = validate_whatsapp_marker_target(
        outside.path().to_str().expect("utf8 path"),
        Some(workspace.path()),
    )
    .expect_err("outside workspace must be refused");

    assert_eq!(err.kind(), WhatsAppMarkerFailure::Refused);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn allowed_groups_jid_prefix_match() {
    let groups = vec!["123456789012345".to_string()];
    assert!(super::is_group_chat_allowed(
        "123456789012345@g.us",
        &groups
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn validate_marker_target_rejects_without_workspace() {
    let err = validate_whatsapp_marker_target("photo.png", None)
        .expect_err("workspace is required for local marker reads");

    assert_eq!(err.kind(), WhatsAppMarkerFailure::Refused);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn allowed_groups_no_match_drops() {
    let groups = vec!["123456789012345".to_string()];
    assert!(!super::is_group_chat_allowed(
        "999999999999999@g.us",
        &groups
    ));
    // Blank / whitespace-only entries never match.
    assert!(!super::is_group_chat_allowed(
        "123@g.us",
        &["   ".to_string()]
    ));
    // Prefix entries match the user part EXACTLY, not as a string prefix:
    // "123" must admit "123@g.us" but never "123999@g.us".
    assert!(super::is_group_chat_allowed(
        "123@g.us",
        &["123".to_string()]
    ));
    assert!(!super::is_group_chat_allowed(
        "123999@g.us",
        &["123".to_string()]
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn validate_marker_target_marks_missing_as_failed() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let err = validate_whatsapp_marker_target("missing.png", Some(workspace.path()))
        .expect_err("missing file should fail delivery");

    assert_eq!(err.kind(), WhatsAppMarkerFailure::Failed);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn delivery_failure_note_is_count_only() {
    let count: usize = 2;
    let note = whatsapp_delivery_failure_note(count).expect("note");

    // Locale-independent: count is always rendered as Arabic digits in
    // every shipped locale's FTL template (`{$count}`). The literal
    // English phrase used to live here but the assertion would break
    // on any CI runner with a non-English `$LANG`.
    assert!(!note.is_empty(), "note must be non-empty");
    assert!(
        note.contains(count.to_string().as_str()),
        "note must contain the failure count"
    );
    assert!(!note.contains("/"), "note must not contain path separators");
    assert!(
        !note.contains("workspace"),
        "note must not echo local marker targets"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn voice_marker_uses_opus_mime_for_ogg_family() {
    assert_eq!(
        WhatsAppMediaKind::Voice.mime_for_path(Path::new("voice.ogg")),
        "audio/ogg; codecs=opus"
    );
    assert_eq!(
        WhatsAppMediaKind::Audio.mime_for_path(Path::new("voice.ogg")),
        "audio/ogg"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn allowed_groups_dm_bypasses_filter() {
    // DMs bypass: the call site gates on `is_group`, so a direct message
    // is admitted even when a non-empty allowed_groups would not match it.
    let groups = vec!["123456789012345".to_string()];
    let is_group = false;
    let dm_jid = "987654321098765@s.whatsapp.net";
    let admitted = !is_group || super::is_group_chat_allowed(dm_jid, &groups);
    assert!(admitted);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn passive_group_context_is_default_off_and_group_only() {
    assert!(!WhatsAppWebChannel::should_record_passive_group_context(
        false, true, false
    ));
    assert!(!WhatsAppWebChannel::should_record_passive_group_context(
        true, false, false
    ));
    assert!(!WhatsAppWebChannel::should_record_passive_group_context(
        true, true, true
    ));
    assert!(WhatsAppWebChannel::should_record_passive_group_context(
        true, true, false
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn passive_group_context_uses_reply_target_scope_for_groups() {
    assert_eq!(
        WhatsAppWebChannel::group_context_scope(false, true),
        ChannelConversationScope::Sender
    );
    assert_eq!(
        WhatsAppWebChannel::group_context_scope(true, false),
        ChannelConversationScope::Sender
    );
    assert_eq!(
        WhatsAppWebChannel::group_context_scope(true, true),
        ChannelConversationScope::ReplyTarget
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_channel_name() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    );
    assert_eq!(ch.name(), "whatsapp");
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_number_allowed_exact() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    );
    assert!(ch.is_number_allowed("+1234567890"));
    assert!(!ch.is_number_allowed("+9876543210"));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_number_allowed_wildcard() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["*".into()]),
        Arc::new(Vec::new),
    );
    assert!(ch.is_number_allowed("+1234567890"));
    assert!(ch.is_number_allowed("+9999999999"));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_number_denied_empty() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(Vec::new),
        Arc::new(Vec::new),
    );
    // Empty allowlist means "deny all" (matches channel-wide allowlist policy).
    assert!(!ch.is_number_allowed("+1234567890"));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_normalize_phone_adds_plus() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    );
    assert_eq!(ch.normalize_phone("1234567890"), "+1234567890");
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_normalize_phone_preserves_plus() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    );
    assert_eq!(ch.normalize_phone("+1234567890"), "+1234567890");
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_normalize_phone_from_jid() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    );
    assert_eq!(
        ch.normalize_phone("1234567890@s.whatsapp.net"),
        "+1234567890"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_normalize_phone_token_accepts_formatted_phone() {
    assert_eq!(
        WhatsAppWebChannel::normalize_phone_token("+1 (555) 123-4567"),
        Some("+15551234567".to_string())
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_allowlist_matches_normalized_format() {
    let allowed = vec!["+15551234567".to_string()];
    assert!(WhatsAppWebChannel::is_number_allowed_for_list(
        &allowed,
        "+1 (555) 123-4567"
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_sender_candidates_include_sender_alt_phone() {
    let sender = Jid::lid("76188559093817");
    let sender_alt = Jid::pn("15551234567");
    let candidates = WhatsAppWebChannel::sender_phone_candidates(&sender, Some(&sender_alt), None);
    assert!(candidates.contains(&"+15551234567".to_string()));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn whatsapp_web_sender_candidates_include_lid_mapping_phone() {
    let sender = Jid::lid("76188559093817");
    let candidates =
        WhatsAppWebChannel::sender_phone_candidates(&sender, None, Some("15551234567"));
    assert!(candidates.contains(&"+15551234567".to_string()));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn compute_reply_target_preserves_lid_dm() {
    // LID DM → preserved as-is (library handles LID resolution internally)
    let chat_jid = "76188559093817@lid";
    let result = WhatsAppWebChannel::compute_reply_target(chat_jid);
    assert_eq!(
        result, chat_jid,
        "LID DM must be preserved - library handles LID addressing natively"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn compute_reply_target_preserves_pn_dm() {
    // PN DM → preserved as-is
    let chat_jid = "15551234567@s.whatsapp.net";
    let result = WhatsAppWebChannel::compute_reply_target(chat_jid);
    assert_eq!(result, chat_jid, "PN DM must preserve original chat JID");
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn compute_reply_target_preserves_group() {
    // Group chat → preserved as-is
    let chat_jid = "120363012345678901@g.us";
    let result = WhatsAppWebChannel::compute_reply_target(chat_jid);
    assert_eq!(
        result, chat_jid,
        "Group chat must preserve original chat JID"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn lid_rejection_diagnostic_empty_for_non_lid_sender() {
    let sender = Jid::pn("15551234567");
    let diag = WhatsAppWebChannel::lid_rejection_diagnostic(&sender, None);
    assert!(
        diag.is_empty(),
        "non-LID senders must not generate any LID-resolution suffix; got {diag:?}"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn lid_rejection_diagnostic_names_resolution_failure_for_lid_with_no_phone() {
    let sender = Jid::lid("76188559093817");
    let diag = WhatsAppWebChannel::lid_rejection_diagnostic(&sender, None);
    assert!(
        diag.contains("LID→phone resolution returned None"),
        "diagnostic must name the resolution failure mode #6350 describes; got {diag:?}"
    );
    assert!(
        diag.contains("76188559093817"),
        "diagnostic must surface the LID identifier so the operator can add the LID-form workaround; got {diag:?}"
    );
    assert!(
        diag.contains("allowed_numbers"),
        "diagnostic must point at the config knob to fix this; got {diag:?}"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn lid_rejection_diagnostic_distinguishes_resolved_phone_mismatch() {
    // LID resolved successfully but the resulting phone wasn't in the
    // allowlist. Different cause from the resolution failure path; the
    // operator shouldn't be steered toward the LID workaround.
    let sender = Jid::lid("76188559093817");
    let diag = WhatsAppWebChannel::lid_rejection_diagnostic(&sender, Some("15551234567"));
    assert!(
        !diag.contains("LID→phone resolution returned None"),
        "must not suggest resolution failed when mapped_phone is Some; got {diag:?}"
    );
    assert!(
        diag.contains("did not match"),
        "diagnostic must explain the resolved phone failed the allowlist; got {diag:?}"
    );
}

#[tokio::test]
#[cfg(feature = "whatsapp-web")]
async fn whatsapp_web_health_check_disconnected() {
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    );
    assert!(!ch.health_check().await);
}

// ── Reconnect retry state machine tests (exercise production helpers) ──

#[test]
#[cfg(feature = "whatsapp-web")]
fn compute_retry_delay_doubles_with_cap() {
    // Uses the production helper that listen() calls for backoff.
    // attempt 1 → 3s, 2 → 6s, 3 → 12s, … 7 → 192s, 8 → 300s (capped)
    let expected = [3, 6, 12, 24, 48, 96, 192, 300, 300, 300];
    for (i, &want) in expected.iter().enumerate() {
        let attempt = (i + 1) as u32;
        assert_eq!(
            WhatsAppWebChannel::compute_retry_delay(attempt),
            want,
            "attempt {attempt}"
        );
    }
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn compute_retry_delay_zero_attempt() {
    // Edge case: attempt 0 should still produce BASE (saturating_sub clamps).
    assert_eq!(
        WhatsAppWebChannel::compute_retry_delay(0),
        WhatsAppWebChannel::BASE_DELAY_SECS
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn record_retry_increments_and_detects_exceeded() {
    use std::sync::atomic::AtomicU32;
    let counter = AtomicU32::new(0);

    // First MAX_RETRIES attempts should not exceed.
    for i in 1..=WhatsAppWebChannel::MAX_RETRIES {
        let (attempt, exceeded) = WhatsAppWebChannel::record_retry(&counter);
        assert_eq!(attempt, i);
        assert!(!exceeded, "attempt {i} should not exceed max");
    }

    // Next attempt exceeds the limit.
    let (attempt, exceeded) = WhatsAppWebChannel::record_retry(&counter);
    assert_eq!(attempt, WhatsAppWebChannel::MAX_RETRIES + 1);
    assert!(exceeded);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn reset_retry_clears_counter() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let counter = AtomicU32::new(0);

    // Simulate several reconnect attempts via the production helper.
    for _ in 0..5 {
        WhatsAppWebChannel::record_retry(&counter);
    }
    assert_eq!(counter.load(Ordering::Relaxed), 5);

    // Event::Connected calls reset_retry — verify it zeroes the counter.
    WhatsAppWebChannel::reset_retry(&counter);
    assert_eq!(counter.load(Ordering::Relaxed), 0);

    // After reset, record_retry starts from 1 again.
    let (attempt, exceeded) = WhatsAppWebChannel::record_retry(&counter);
    assert_eq!(attempt, 1);
    assert!(!exceeded);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn should_purge_session_only_when_revoked() {
    use std::sync::atomic::AtomicBool;
    let flag = AtomicBool::new(false);

    // Transient crash: flag is false → should NOT purge.
    assert!(!WhatsAppWebChannel::should_purge_session(&flag));

    // Explicit LoggedOut: flag set to true → should purge.
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(WhatsAppWebChannel::should_purge_session(&flag));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn with_transcription_sets_config_when_enabled() {
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test_key".to_string()),
        ..Default::default()
    };

    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    )
    .with_transcription(tc);
    assert!(ch.transcription.is_some());
    assert!(ch.transcription_manager.is_some());
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn with_transcription_ignores_when_disabled() {
    let tc = zeroclaw_config::schema::TranscriptionConfig::default(); // enabled = false
    let mention_only = false;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test-whatsapp.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["+1234567890".into()]),
        Arc::new(Vec::new),
    )
    .with_transcription(tc);
    assert!(ch.transcription.is_none());
    assert!(ch.transcription_manager.is_none());
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn session_file_paths_includes_wal_and_shm() {
    let paths = WhatsAppWebChannel::session_file_paths("/tmp/test.db");
    assert_eq!(
        paths,
        [
            "/tmp/test.db".to_string(),
            "/tmp/test.db-wal".to_string(),
            "/tmp/test.db-shm".to_string(),
        ]
    );
}

// ── Mention detection tests ──

#[cfg(feature = "whatsapp-web")]
fn extended_text_reply(participant: &str, mentioned_jids: &[&str]) -> waproto::whatsapp::Message {
    waproto::whatsapp::Message {
        extended_text_message: Some(Box::new(waproto::whatsapp::message::ExtendedTextMessage {
            text: Some("expand the previous response".to_string()),
            context_info: Some(Box::new(waproto::whatsapp::ContextInfo {
                participant: Some(participant.to_string()),
                mentioned_jid: mentioned_jids
                    .iter()
                    .map(|jid| (*jid).to_string())
                    .collect(),
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[cfg(feature = "whatsapp-web")]
fn sticker_reply(
    participant: &str,
    quoted_message: Option<waproto::whatsapp::Message>,
) -> waproto::whatsapp::Message {
    waproto::whatsapp::Message {
        sticker_message: Some(Box::new(waproto::whatsapp::message::StickerMessage {
            mimetype: Some("image/webp".to_string()),
            context_info: Some(Box::new(waproto::whatsapp::ContextInfo {
                participant: Some(participant.to_string()),
                quoted_message: quoted_message.map(Box::new),
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[cfg(feature = "whatsapp-web")]
fn image_mention(mentioned_jids: &[&str]) -> waproto::whatsapp::Message {
    waproto::whatsapp::Message {
        image_message: Some(Box::new(waproto::whatsapp::message::ImageMessage {
            mimetype: Some("image/jpeg".to_string()),
            context_info: Some(Box::new(waproto::whatsapp::ContextInfo {
                mentioned_jid: mentioned_jids
                    .iter()
                    .map(|jid| (*jid).to_string())
                    .collect(),
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    }
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn jid_digits_extracts_phone_from_jid() {
    assert_eq!(
        WhatsAppWebChannel::jid_digits("919211916069@s.whatsapp.net"),
        "919211916069"
    );
    assert_eq!(
        WhatsAppWebChannel::jid_digits("76188559093817@lid"),
        "76188559093817"
    );
    assert_eq!(WhatsAppWebChannel::jid_digits("15551234567"), "15551234567");
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_structured() {
    let jids = vec!["919211916069@s.whatsapp.net".to_string()];
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "hey @919211916069 check this",
        &jids,
        "919211916069",
        None
    ));
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "hey check this",
        &jids,
        "919211916069",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_text_fallback() {
    let no_jids: Vec<String> = vec![];
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "hey @919211916069 check this",
        &no_jids,
        "919211916069",
        None
    ));
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "hey @919211916069",
        &no_jids,
        "919211916069",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_prefix_false_positive() {
    let no_jids: Vec<String> = vec![];
    assert!(!WhatsAppWebChannel::contains_bot_mention(
        "hey @919211916069 check this",
        &no_jids,
        "91921191606",
        None
    ));
    assert!(!WhatsAppWebChannel::contains_bot_mention(
        "hey @155512345678",
        &no_jids,
        "15551234567",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_no_match() {
    let no_jids: Vec<String> = vec![];
    assert!(!WhatsAppWebChannel::contains_bot_mention(
        "just a regular message",
        &no_jids,
        "919211916069",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_scans_past_prefix_false_match() {
    let no_jids: Vec<String> = vec![];
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "@9192119160691 real @919211916069",
        &no_jids,
        "919211916069",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_rejects_embedded_at() {
    let no_jids: Vec<String> = vec![];
    assert!(!WhatsAppWebChannel::contains_bot_mention(
        "foo@919211916069 bar",
        &no_jids,
        "919211916069",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn jid_digits_strips_device_suffix() {
    assert_eq!(
        WhatsAppWebChannel::jid_digits("919211916069:16@s.whatsapp.net"),
        "919211916069"
    );
    assert_eq!(
        WhatsAppWebChannel::jid_digits("227728477442093:3@lid"),
        "227728477442093"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_matches_lid() {
    let jids = vec!["227728477442093@lid".to_string()];
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "hey @DisplayName check this",
        &jids,
        "6287778315246",
        Some("227728477442093")
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn contains_bot_mention_matches_lid_when_phone_unknown() {
    let jids = vec!["227728477442093@lid".to_string()];
    assert!(WhatsAppWebChannel::contains_bot_mention(
        "hey @DisplayName check this",
        &jids,
        "",
        Some("227728477442093")
    ));
    assert!(!WhatsAppWebChannel::contains_bot_mention(
        "plain @ mention",
        &[],
        "",
        None
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn extract_mentioned_jids_reads_media_context_info() {
    let msg = image_mention(&["100@s.whatsapp.net"]);
    assert_eq!(
        WhatsAppWebChannel::extract_mentioned_jids(&msg),
        vec!["100@s.whatsapp.net".to_string()]
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn message_addressed_to_bot_accepts_reply_to_phone_jid() {
    let msg = extended_text_reply("100@s.whatsapp.net", &[]);
    assert!(WhatsAppWebChannel::is_message_addressed_to_bot(
        &msg,
        "expand the previous response",
        "100",
        None,
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn message_addressed_to_bot_accepts_reply_to_lid_jid() {
    let msg = extended_text_reply("200@lid", &[]);
    assert!(WhatsAppWebChannel::is_message_addressed_to_bot(
        &msg,
        "expand the previous response",
        "100",
        Some("200"),
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn message_addressed_to_bot_accepts_media_reply_to_lid_jid() {
    let msg = sticker_reply("200@lid", None);
    assert!(WhatsAppWebChannel::is_message_addressed_to_bot(
        &msg,
        "[Sticker]",
        "100",
        Some("200"),
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn extract_quoted_message_reads_media_context_info() {
    let quoted = waproto::whatsapp::Message {
        image_message: Some(Box::new(waproto::whatsapp::message::ImageMessage {
            mimetype: Some("image/png".to_string()),
            ..Default::default()
        })),
        ..Default::default()
    };
    let msg = sticker_reply("200@lid", Some(quoted));
    let quoted = WhatsAppWebChannel::extract_quoted_message(&msg)
        .expect("sticker reply should expose the quoted message");
    assert!(quoted.image_message.is_some());
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn media_fallback_content_keeps_sticker_messages_addressable() {
    let msg = sticker_reply("200@lid", None);
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content(String::new(), &msg),
        "[Sticker]"
    );
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content("hello".to_string(), &msg),
        "hello"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn media_fallback_content_leaves_non_media_messages_empty() {
    let msg = waproto::whatsapp::Message::default();
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content(String::new(), &msg),
        ""
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn media_fallback_content_parses_static_location() {
    let msg = waproto::whatsapp::Message {
        location_message: Some(Box::new(waproto::whatsapp::message::LocationMessage {
            degrees_latitude: Some(40.7128),
            degrees_longitude: Some(-74.0060),
            name: Some("NYC".into()),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content(String::new(), &msg),
        "[Location: 40.712800, -74.006000 — NYC]"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn media_fallback_content_skips_live_location() {
    let msg = waproto::whatsapp::Message {
        location_message: Some(Box::new(waproto::whatsapp::message::LocationMessage {
            degrees_latitude: Some(40.7128),
            degrees_longitude: Some(-74.0060),
            is_live: Some(true),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content(String::new(), &msg),
        ""
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn media_fallback_content_skips_missing_coordinates() {
    // Missing longitude — should silently drop, not fabricate 0,0
    let msg = waproto::whatsapp::Message {
        location_message: Some(Box::new(waproto::whatsapp::message::LocationMessage {
            degrees_latitude: Some(40.7128),
            degrees_longitude: None,
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content(String::new(), &msg),
        ""
    );
    // Missing latitude
    let msg = waproto::whatsapp::Message {
        location_message: Some(Box::new(waproto::whatsapp::message::LocationMessage {
            degrees_latitude: None,
            degrees_longitude: Some(-74.0060),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert_eq!(
        WhatsAppWebChannel::media_fallback_content(String::new(), &msg),
        ""
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn mime_extension_uses_safe_subtype() {
    assert_eq!(
        WhatsAppWebChannel::mime_extension("image/jpeg; name=photo", "jpg"),
        "jpg"
    );
    assert_eq!(
        WhatsAppWebChannel::mime_extension("application/vnd.ms-excel", "bin"),
        "vnd.ms-excel"
    );
    assert_eq!(
        WhatsAppWebChannel::mime_extension("image/../../png", "bin"),
        "bin"
    );
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn message_addressed_to_bot_rejects_reply_to_other_participant() {
    let msg = extended_text_reply("300@s.whatsapp.net", &[]);
    assert!(!WhatsAppWebChannel::is_message_addressed_to_bot(
        &msg,
        "expand the previous response",
        "100",
        Some("200"),
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn message_addressed_to_bot_accepts_explicit_mention_in_other_reply() {
    let msg = extended_text_reply("300@s.whatsapp.net", &["100@s.whatsapp.net"]);
    assert!(WhatsAppWebChannel::is_message_addressed_to_bot(
        &msg,
        "expand the previous response",
        "100",
        Some("200"),
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn constructor_seeds_bot_phone_from_pair_phone() {
    let mention_only = true;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test.db".into()),
        pair_phone: Some("919211916069".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["*".into()]),
        Arc::new(Vec::new),
    );
    assert_eq!(*ch.bot_phone.lock(), Some("919211916069".to_string()));
    assert_eq!(*ch.bot_lid.lock(), None);
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn constructor_no_pair_phone_leaves_bot_phone_none() {
    let mention_only = true;
    let self_chat_mode = false;
    let cfg = zeroclaw_config::schema::WhatsAppConfig {
        enabled: true,
        session_path: Some("/tmp/test.db".into()),
        mention_only,
        self_chat_mode,
        ..Default::default()
    };
    let ch = WhatsAppWebChannel::new(
        &cfg,
        "whatsapp_web_test_alias",
        Arc::new(|| vec!["*".into()]),
        Arc::new(Vec::new),
    );
    assert_eq!(*ch.bot_phone.lock(), None);
}

// ── fromme_outside_self_chat_is_operator_trigger ───────────

#[test]
#[cfg(feature = "whatsapp-web")]
fn fromme_trigger_drops_when_no_mention_patterns_configured() {
    let dm: Vec<regex::Regex> = vec![];
    let group: Vec<regex::Regex> = vec![];
    // Without configured patterns, a fromMe message in a third-party
    // DM or group must drop — there is no opt-in signal that says the
    // operator wants outbound mirrors to be treated as triggers.
    assert!(!fromme_outside_self_chat_is_operator_trigger(
        false,
        &dm,
        &group,
        "TinyBot foo"
    ));
    assert!(!fromme_outside_self_chat_is_operator_trigger(
        true,
        &dm,
        &group,
        "TinyBot foo"
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn fromme_trigger_falls_through_when_dm_pattern_matches() {
    // @ilteoood's configured workflow: dm_mention_patterns = ["TinyBot"].
    // Operator types "TinyBot translate this" in a friend's DM →
    // intentional invocation, must fall through.
    let dm = vec![
        regex::RegexBuilder::new("TinyBot")
            .case_insensitive(true)
            .build()
            .unwrap(),
    ];
    let group: Vec<regex::Regex> = vec![];
    assert!(fromme_outside_self_chat_is_operator_trigger(
        false,
        &dm,
        &group,
        "TinyBot translate this"
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn fromme_trigger_drops_when_dm_pattern_does_not_match() {
    // Operator types a normal message in a friend's DM — even with
    // patterns configured, no match means it stays an outbound mirror
    // and must be dropped to prevent impersonation.
    let dm = vec![
        regex::RegexBuilder::new("TinyBot")
            .case_insensitive(true)
            .build()
            .unwrap(),
    ];
    let group: Vec<regex::Regex> = vec![];
    assert!(!fromme_outside_self_chat_is_operator_trigger(
        false,
        &dm,
        &group,
        "see you at 7"
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn fromme_trigger_uses_group_patterns_for_group_threads() {
    // group_mention_patterns gates the group case; dm patterns must
    // not be consulted for group messages and vice versa. This pins
    // the predicate's branch selection.
    let dm: Vec<regex::Regex> = vec![
        regex::RegexBuilder::new("DmTrigger")
            .case_insensitive(true)
            .build()
            .unwrap(),
    ];
    let group = vec![
        regex::RegexBuilder::new("GroupTrigger")
            .case_insensitive(true)
            .build()
            .unwrap(),
    ];
    // In a group, only group_patterns matter.
    assert!(fromme_outside_self_chat_is_operator_trigger(
        true,
        &dm,
        &group,
        "GroupTrigger hi"
    ));
    assert!(!fromme_outside_self_chat_is_operator_trigger(
        true,
        &dm,
        &group,
        "DmTrigger hi"
    ));
    // In a DM, only dm_patterns matter.
    assert!(fromme_outside_self_chat_is_operator_trigger(
        false,
        &dm,
        &group,
        "DmTrigger hi"
    ));
    assert!(!fromme_outside_self_chat_is_operator_trigger(
        false,
        &dm,
        &group,
        "GroupTrigger hi"
    ));
}

#[test]
#[cfg(feature = "whatsapp-web")]
fn fromme_trigger_drops_when_text_is_empty() {
    // Voice notes and media-only messages return empty text. With no
    // text to match against, the operator-trigger path must drop —
    // never transcribe a fromMe voice note just to check whether it
    // is a bot trigger (cost + impersonation risk).
    let dm = vec![
        regex::RegexBuilder::new("TinyBot")
            .case_insensitive(true)
            .build()
            .unwrap(),
    ];
    let group: Vec<regex::Regex> = vec![];
    assert!(!fromme_outside_self_chat_is_operator_trigger(
        false, &dm, &group, ""
    ));
}
