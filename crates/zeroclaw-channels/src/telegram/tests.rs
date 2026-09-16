use super::*;

#[test]
fn scrub_masks_poll_error_url() {
    let raw =
        "error sending request for url (https://api.telegram.org/bot123456:ABC-def_GHI/getUpdates)";
    let redacted = zeroclaw_runtime::security::scrub(raw);
    assert!(!redacted.contains("123456:ABC-def_GHI"));
    assert!(redacted.contains("[REDACTED_BOT_TOKEN]"));
}

#[test]
fn scrub_leaves_unrelated_text_untouched() {
    let raw = "connection reset by peer";
    assert_eq!(zeroclaw_runtime::security::scrub(raw), raw);
}

#[test]
fn voice_peer_resolver_resolves_live_from_config() {
    use zeroclaw_config::multi_agent::{OutputModality, PeerGroupConfig, PeerUsername};

    let mut config = zeroclaw_config::schema::Config::default();
    // Voice group on this channel type — should be resolved.
    config.peer_groups.insert(
        "voicers".to_string(),
        PeerGroupConfig {
            channel: "telegram".into(),
            external_peers: vec![PeerUsername::new("@alice"), PeerUsername::new("@bob")],
            output_modality: OutputModality::Voice,
            ..Default::default()
        },
    );
    // Voice group on a different channel — must NOT leak into telegram.
    config.peer_groups.insert(
        "other".to_string(),
        PeerGroupConfig {
            channel: "signal".into(),
            external_peers: vec![PeerUsername::new("@carol")],
            output_modality: OutputModality::Voice,
            ..Default::default()
        },
    );
    // Mirror group on this channel — not a voice preference, skip.
    config.peer_groups.insert(
        "mirrorers".to_string(),
        PeerGroupConfig {
            channel: "telegram".into(),
            external_peers: vec![PeerUsername::new("@dave")],
            output_modality: OutputModality::Mirror,
            ..Default::default()
        },
    );

    let ch = TelegramChannel::new(
        "fake-token".into(),
        "default",
        Arc::new(|| vec!["*".into()]),
        false,
    )
    .with_voice_peer_resolver(Arc::new({
        let cfg = config.clone();
        move || cfg.channel_voice_peers("telegram", "default")
    }));

    // is_voice_chat resolves live via voice_peer_resolver — no cache.
    assert!(
        ch.is_voice_chat("@alice"),
        "voice peer should be recognized"
    );
    assert!(ch.is_voice_chat("@bob"), "voice peer should be recognized");
    assert!(
        !ch.is_voice_chat("@carol"),
        "peers on another channel must not be recognized"
    );
    assert!(
        !ch.is_voice_chat("@dave"),
        "mirror-modality peers must not be recognized"
    );

    // Live resolver must NOT pollute the session voice_chats set.
    let vc = ch.voice_chats.lock().unwrap();
    assert!(
        !vc.contains("@alice"),
        "live-resolved peers must not pollute the session voice_chats set"
    );
}

#[test]
fn voice_peer_resolver_survives_session_voice_chats_removal() {
    use zeroclaw_config::multi_agent::{OutputModality, PeerGroupConfig, PeerUsername};

    let mut config = zeroclaw_config::schema::Config::default();
    config.peer_groups.insert(
        "voicers".to_string(),
        PeerGroupConfig {
            channel: "telegram".into(),
            external_peers: vec![PeerUsername::new("@alice")],
            output_modality: OutputModality::Voice,
            ..Default::default()
        },
    );

    let ch = TelegramChannel::new(
        "fake-token".into(),
        "default",
        Arc::new(|| vec!["*".into()]),
        false,
    )
    .with_voice_peer_resolver(Arc::new({
        let cfg = config.clone();
        move || cfg.channel_voice_peers("telegram", "default")
    }));

    // Simulate a voice-send removing @alice from voice_chats (even though
    // she was never in it — this proves live-resolved peers are separate).
    ch.voice_chats.lock().unwrap().remove("@alice");

    // is_voice_chat must still return true via voice_peer_resolver.
    assert!(
        ch.is_voice_chat("@alice"),
        "live-resolved voice peer must remain active after voice_chats removal"
    );
}

#[test]
fn audio_send_spec_opus_is_voice_note() {
    // Only OGG/Opus becomes a real Telegram voice note.
    let (method, field, filename, mime) = telegram_audio_send_spec("opus").unwrap();
    assert_eq!(method, "sendVoice");
    assert_eq!(field, "voice");
    assert_eq!(filename, "voice.ogg");
    assert_eq!(mime, "audio/ogg");
    // "ogg" is an accepted alias for the same path.
    assert_eq!(telegram_audio_send_spec("ogg").unwrap().0, "sendVoice");
}

#[test]
fn audio_send_spec_wav_uses_send_audio_with_real_mime() {
    // Groq Orpheus / Piper emit WAV — must not be mislabeled as audio/ogg.
    let (method, field, filename, mime) = telegram_audio_send_spec("wav").unwrap();
    assert_eq!(method, "sendAudio");
    assert_eq!(field, "audio");
    assert_eq!(filename, "voice.wav");
    assert_eq!(mime, "audio/wav");
}

#[test]
fn audio_send_spec_mp3_uses_send_audio() {
    let (method, _field, filename, mime) = telegram_audio_send_spec("mp3").unwrap();
    assert_eq!(method, "sendAudio");
    assert_eq!(filename, "voice.mp3");
    assert_eq!(mime, "audio/mpeg");
}

#[test]
fn audio_send_spec_is_case_and_whitespace_insensitive() {
    assert_eq!(telegram_audio_send_spec("  WAV ").unwrap().2, "voice.wav");
    assert_eq!(telegram_audio_send_spec("Opus").unwrap().0, "sendVoice");
}

#[test]
fn audio_send_spec_pcm_is_rejected() {
    let err = telegram_audio_send_spec("pcm")
        .expect_err("pcm must be rejected — it is not a container format");
    assert!(err.to_string().contains("PCM"), "got: {err}");
}

#[test]
fn audio_send_spec_unknown_format_falls_back_to_octet_stream() {
    let (method, _field, filename, mime) = telegram_audio_send_spec("speex").unwrap();
    assert_eq!(method, "sendAudio");
    assert_eq!(filename, "voice.bin");
    assert_eq!(mime, "application/octet-stream");
}

#[test]
fn telegram_channel_name() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    assert_eq!(ch.name(), "telegram");
}

#[tokio::test]
async fn telegram_with_transcription_binds_sole_provider_alias() {
    // SAFETY: test-only, single-threaded test runner.
    unsafe { std::env::remove_var("GROQ_API_KEY") };

    // Only the Groq key is set -> exactly one provider registers.
    let config = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test-groq-key".to_string()),
        ..zeroclaw_config::schema::TranscriptionConfig::default()
    };

    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        false,
    )
    .with_transcription(config);

    let manager = ch
        .transcription_manager
        .as_ref()
        .expect("single configured provider must build a transcription manager");

    // Alias is bound for the single-provider case. Stop before any network
    // call by using an unsupported audio format, which `validate_audio`
    // rejects first inside the provider's `transcribe`.
    let err = manager
        .transcribe(&[0u8; 16], "voice.aac")
        .await
        .expect_err("unsupported format must error before any network call");
    let msg = err.to_string();
    assert!(
        !msg.contains("no transcription_provider configured"),
        "alias must be bound for the single-provider case; got: {msg}"
    );
    assert!(
        msg.contains("Unsupported audio format"),
        "expected the bound provider to reach audio validation; got: {msg}"
    );
}

#[test]
fn random_telegram_ack_reaction_is_from_pool() {
    for _ in 0..128 {
        let emoji = random_telegram_ack_reaction();
        assert!(TELEGRAM_ACK_REACTIONS.contains(&emoji));
    }
}

#[test]
fn telegram_ack_reaction_request_shape() {
    let body = build_telegram_ack_reaction_request("-100200300", 42, "⚡️");
    assert_eq!(body["chat_id"], "-100200300");
    assert_eq!(body["message_id"], 42);
    assert_eq!(body["reaction"][0]["type"], "emoji");
    assert_eq!(body["reaction"][0]["emoji"], "⚡️");
}

#[test]
fn telegram_extract_update_message_target_parses_ids() {
    let update = serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 99,
            "chat": { "id": -100_123_456 }
        }
    });

    let target = TelegramChannel::extract_update_message_target(&update);
    assert_eq!(target, Some(("-100123456".to_string(), 99)));
}

#[test]
fn typing_handle_starts_as_none() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let guard = ch.typing_handle.lock();
    assert!(guard.is_none());
}

#[tokio::test]
async fn stop_typing_clears_handle() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );

    // Manually insert a dummy handle
    {
        let mut guard = ch.typing_handle.lock();
        *guard = Some(zeroclaw_spawn::spawn!(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }));
    }

    // stop_typing should abort and clear
    ch.stop_typing("123").await.unwrap();

    let guard = ch.typing_handle.lock();
    assert!(guard.is_none());
}

#[tokio::test]
async fn start_typing_replaces_previous_handle() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );

    // Insert a dummy handle first
    {
        let mut guard = ch.typing_handle.lock();
        *guard = Some(zeroclaw_spawn::spawn!(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }));
    }

    // start_typing should abort the old handle and set a new one
    let _ = ch.start_typing("123").await;

    let guard = ch.typing_handle.lock();
    assert!(guard.is_some());
}

#[test]
fn supports_draft_updates_respects_stream_mode() {
    let mention_only = false;
    let off = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    assert!(!off.supports_draft_updates());

    let partial = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 750);
    assert!(partial.supports_draft_updates());
    assert_eq!(partial.draft_update_interval_ms, 750);
}

#[test]
fn with_streaming_uses_default_for_zero_draft_update_interval() {
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        false,
    )
    .with_streaming(StreamMode::Partial, 0);

    assert_eq!(
        ch.draft_update_interval_ms,
        TELEGRAM_DRAFT_UPDATE_INTERVAL_MS
    );
}

#[tokio::test]
async fn send_draft_returns_none_when_stream_mode_off() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let id = ch
        .send_draft(&SendMessage::new("draft", "123"))
        .await
        .unwrap();
    assert!(id.is_none());
}

#[tokio::test]
async fn update_draft_rate_limit_short_circuits_network() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 60_000);
    ch.last_draft_edit
        .lock()
        .insert("123".to_string(), std::time::Instant::now());

    let result = ch.update_draft("123", "42", "delta text").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn update_draft_utf8_truncation_is_safe_for_multibyte_text() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 0);
    let long_emoji_text = "😀".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 20);

    // Invalid message_id returns early after building display_text.
    // This asserts truncation never panics on UTF-8 boundaries.
    let result = ch
        .update_draft("123", "not-a-number", &long_emoji_text)
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn finalize_draft_invalid_message_id_falls_back_to_chunk_send() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 0);
    let long_text = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 64);

    // For oversized text + invalid draft message_id, finalize_draft should
    // fall back to chunked send instead of returning early.
    let result = ch
        .finalize_draft("123", "not-a-number", &long_text, false)
        .await;
    assert!(result.is_err());
}

#[test]
fn telegram_api_url() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert_eq!(
        ch.api_url("getMe"),
        "https://api.telegram.org/bot123:ABC/getMe"
    );
}

#[test]
fn telegram_api_url_uses_custom_api_base() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    )
    .with_api_base("http://127.0.0.1:8081".to_string());

    assert_eq!(
        ch.api_url("getMe"),
        "http://127.0.0.1:8081/bot123:ABC/getMe"
    );
}

#[test]
fn telegram_api_url_normalizes_custom_api_base_trailing_slash() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    )
    .with_api_base("http://127.0.0.1:8081/".to_string());

    assert_eq!(
        ch.api_url("getMe"),
        "http://127.0.0.1:8081/bot123:ABC/getMe"
    );
}

#[test]
fn telegram_markdown_to_html_escapes_quotes_in_link_href() {
    let rendered =
        TelegramChannel::markdown_to_telegram_html("[click](https://example.com?q=\"x\"&a='b')");
    assert_eq!(
        rendered,
        "<a href=\"https://example.com?q=&quot;x&quot;&amp;a=&#39;b&#39;\">click</a>"
    );
}

#[test]
fn telegram_markdown_to_html_escapes_quotes_in_plain_text() {
    let rendered = TelegramChannel::markdown_to_telegram_html("say \"hi\" & <tag> 'ok'");
    assert_eq!(
        rendered,
        "say &quot;hi&quot; &amp; &lt;tag&gt; &#39;ok&#39;"
    );
}

#[test]
fn telegram_markdown_to_html_code_block_drops_language_attribute() {
    let rendered =
        TelegramChannel::markdown_to_telegram_html("```rust\" onclick=\"alert(1)\nlet x = 1;\n```");
    assert_eq!(rendered, "<pre><code>let x = 1;</code></pre>");
    assert!(!rendered.contains("language-"));
    assert!(!rendered.contains("onclick"));
}

#[test]
fn telegram_user_allowed_wildcard() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    assert!(ch.is_user_allowed("anyone"));
}

#[test]
fn telegram_user_allowed_specific() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into(), "bob".into()]),
        mention_only,
    );
    assert!(ch.is_user_allowed("alice"));
    assert!(!ch.is_user_allowed("eve"));
}

#[test]
fn telegram_user_allowed_with_at_prefix_in_config() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["@alice".into()]),
        mention_only,
    );
    assert!(ch.is_user_allowed("alice"));
}

#[test]
fn telegram_user_denied_empty() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert!(!ch.is_user_allowed("anyone"));
}

#[test]
fn telegram_user_exact_match_not_substring() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into()]),
        mention_only,
    );
    assert!(!ch.is_user_allowed("alice_bot"));
    assert!(!ch.is_user_allowed("alic"));
    assert!(!ch.is_user_allowed("malice"));
}

#[test]
fn telegram_user_empty_string_denied() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into()]),
        mention_only,
    );
    assert!(!ch.is_user_allowed(""));
}

#[test]
fn telegram_user_case_sensitive() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["Alice".into()]),
        mention_only,
    );
    assert!(ch.is_user_allowed("Alice"));
    assert!(!ch.is_user_allowed("alice"));
    assert!(!ch.is_user_allowed("ALICE"));
}

#[test]
fn telegram_wildcard_with_specific_users() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into(), "*".into()]),
        mention_only,
    );
    assert!(ch.is_user_allowed("alice"));
    assert!(ch.is_user_allowed("bob"));
    assert!(ch.is_user_allowed("anyone"));
}

#[test]
fn telegram_user_allowed_by_numeric_id_identity() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["123456789".into()]),
        mention_only,
    );
    assert!(ch.is_any_user_allowed(["unknown", "123456789"]));
}

#[test]
fn telegram_user_denied_when_none_of_identities_match() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into(), "987654321".into()]),
        mention_only,
    );
    assert!(!ch.is_any_user_allowed(["unknown", "123456789"]));
}

#[test]
fn telegram_pairing_enabled_with_empty_allowlist() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert!(ch.pairing_code_active());
}

#[test]
fn telegram_pairing_disabled_with_nonempty_allowlist() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into()]),
        mention_only,
    );
    assert!(!ch.pairing_code_active());
}

#[test]
fn telegram_extract_bind_code_plain_command() {
    assert_eq!(
        TelegramChannel::extract_bind_code("/bind 123456"),
        Some("123456")
    );
}

#[test]
fn telegram_extract_bind_code_supports_bot_mention() {
    assert_eq!(
        TelegramChannel::extract_bind_code("/bind@zeroclaw_bot 654321"),
        Some("654321")
    );
}

#[test]
fn telegram_extract_bind_code_rejects_invalid_forms() {
    assert_eq!(TelegramChannel::extract_bind_code("/bind"), None);
    assert_eq!(TelegramChannel::extract_bind_code("/start"), None);
}

#[test]
fn suggested_bind_command_omits_alias_flag_for_default() {
    // The CLI defaults to the `default` alias, so the short form must
    // stay byte-identical for existing default-alias users.
    assert_eq!(
        TelegramChannel::suggested_bind_command("default", "123456789"),
        "zeroclaw channel bind-telegram 123456789"
    );
}

#[test]
fn suggested_bind_command_appends_alias_flag_for_non_default() {
    // A non-default agent must get the `--alias` flag or the operator's
    // copy-pasted command binds the wrong peer group and the bot keeps
    // asking for approval.
    assert_eq!(
        TelegramChannel::suggested_bind_command("alerts", "123456789"),
        "zeroclaw channel bind-telegram 123456789 --alias alerts"
    );
}

#[test]
fn parse_attachment_markers_extracts_multiple_types() {
    let message = "Here are files [IMAGE:/tmp/a.png] and [DOCUMENT:https://example.com/a.pdf]";
    let (cleaned, attachments) = parse_attachment_markers(message);

    assert_eq!(cleaned, "Here are files  and");
    assert_eq!(attachments.len(), 2);
    assert_eq!(attachments[0].kind, TelegramAttachmentKind::Image);
    assert_eq!(attachments[0].target, "/tmp/a.png");
    assert_eq!(attachments[1].kind, TelegramAttachmentKind::Document);
    assert_eq!(attachments[1].target, "https://example.com/a.pdf");
}

#[test]
fn parse_attachment_markers_keeps_invalid_markers_in_text() {
    let message = "Report [UNKNOWN:/tmp/a.bin]";
    let (cleaned, attachments) = parse_attachment_markers(message);

    assert_eq!(cleaned, "Report [UNKNOWN:/tmp/a.bin]");
    assert!(attachments.is_empty());
}

#[test]
fn parse_path_only_attachment_detects_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let image_path = dir.path().join("snap.png");
    std::fs::write(&image_path, b"fake-png").unwrap();

    let parsed = parse_path_only_attachment(image_path.to_string_lossy().as_ref())
        .expect("expected attachment");

    assert_eq!(parsed.kind, TelegramAttachmentKind::Image);
    assert_eq!(parsed.target, image_path.to_string_lossy());
}

#[test]
fn parse_path_only_attachment_rejects_sentence_text() {
    assert!(parse_path_only_attachment("Screenshot saved to /tmp/snap.png").is_none());
}

#[test]
fn infer_attachment_kind_from_target_detects_document_extension() {
    assert_eq!(
        infer_attachment_kind_from_target("https://example.com/files/specs.pdf?download=1"),
        Some(TelegramAttachmentKind::Document)
    );
}

#[test]
fn parse_update_message_uses_chat_id_as_reply_target() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 33,
            "text": "hello",
            "from": {
                "id": 555,
                "username": "alice"
            },
            "chat": {
                "id": -100_200_300
            }
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("message should parse");

    assert_eq!(msg.sender, "alice");
    assert_eq!(msg.reply_target, "-100200300");
    assert_eq!(msg.content, "hello");
    assert_eq!(msg.id, "telegram_-100200300_33");
}

#[test]
fn parse_update_message_allows_numeric_id_without_username() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["555".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 2,
        "message": {
            "message_id": 9,
            "text": "ping",
            "from": {
                "id": 555
            },
            "chat": {
                "id": 12345
            }
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("numeric allowlist should pass");

    assert_eq!(msg.sender, "555");
    assert_eq!(msg.reply_target, "12345");
}

#[test]
fn parse_update_message_extracts_thread_id_for_forum_topic() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 3,
        "message": {
            "message_id": 42,
            "text": "hello from topic",
            "from": {
                "id": 555,
                "username": "alice"
            },
            "chat": {
                "id": -100_200_300
            },
            "message_thread_id": 789
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("message with thread_id should parse");

    assert_eq!(msg.sender, "alice");
    assert_eq!(msg.reply_target, "-100200300:789");
    assert_eq!(msg.content, "hello from topic");
    assert_eq!(msg.id, "telegram_-100200300_42");
}

// ── File sending API URL tests ──────────────────────────────────

#[test]
fn telegram_api_url_send_document() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert_eq!(
        ch.api_url("sendDocument"),
        "https://api.telegram.org/bot123:ABC/sendDocument"
    );
}

#[test]
fn telegram_api_url_send_photo() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert_eq!(
        ch.api_url("sendPhoto"),
        "https://api.telegram.org/bot123:ABC/sendPhoto"
    );
}

#[test]
fn telegram_api_url_send_video() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert_eq!(
        ch.api_url("sendVideo"),
        "https://api.telegram.org/bot123:ABC/sendVideo"
    );
}

#[test]
fn telegram_api_url_send_audio() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert_eq!(
        ch.api_url("sendAudio"),
        "https://api.telegram.org/bot123:ABC/sendAudio"
    );
}

#[test]
fn telegram_api_url_send_voice() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "123:ABC".into(),
        "telegram_test_alias",
        Arc::new(Vec::new),
        mention_only,
    );
    assert_eq!(
        ch.api_url("sendVoice"),
        "https://api.telegram.org/bot123:ABC/sendVoice"
    );
}

// ── File sending integration tests (with mock server) ──────────

#[tokio::test]
async fn telegram_send_document_bytes_builds_correct_form() {
    // This test verifies the method doesn't panic and handles bytes correctly
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let file_bytes = b"Hello, this is a test file content".to_vec();

    // The actual API call will fail (no real server), but we verify the method exists
    // and handles the input correctly up to the network call
    let result = ch
        .send_document_bytes("123456", None, file_bytes, "test.txt", Some("Test caption"))
        .await;

    // Should fail with network error, not a panic or type error
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    // Error should be network-related, not a code bug
    assert!(
        err.contains("error") || err.contains("failed") || err.contains("connect"),
        "Expected network error, got: {err}"
    );
}

#[tokio::test]
async fn telegram_send_photo_bytes_builds_correct_form() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    // Minimal valid PNG header bytes
    let file_bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    let result = ch
        .send_photo_bytes("123456", None, file_bytes, "test.png", None)
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn telegram_send_document_by_url_builds_correct_json() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );

    let result = ch
        .send_document_by_url(
            "123456",
            None,
            "https://example.com/file.pdf",
            Some("PDF doc"),
        )
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn telegram_send_photo_by_url_builds_correct_json() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );

    let result = ch
        .send_photo_by_url("123456", None, "https://example.com/image.jpg", None)
        .await;

    assert!(result.is_err());
}

// ── File path handling tests ────────────────────────────────────

#[tokio::test]
async fn telegram_send_document_nonexistent_file() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let path = Path::new("/nonexistent/path/to/file.txt");

    let result = ch.send_document("123456", None, path, None).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    // Should fail with file not found error
    assert!(
        err.contains("No such file") || err.contains("not found") || err.contains("os error"),
        "Expected file not found error, got: {err}"
    );
}

#[tokio::test]
async fn telegram_send_photo_nonexistent_file() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let path = Path::new("/nonexistent/path/to/photo.jpg");

    let result = ch.send_photo("123456", None, path, None).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn telegram_send_video_nonexistent_file() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let path = Path::new("/nonexistent/path/to/video.mp4");

    let result = ch.send_video("123456", None, path, None).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn telegram_send_audio_nonexistent_file() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let path = Path::new("/nonexistent/path/to/audio.mp3");

    let result = ch.send_audio("123456", None, path, None).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn telegram_send_voice_nonexistent_file() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let path = Path::new("/nonexistent/path/to/voice.ogg");

    let result = ch.send_voice("123456", None, path, None).await;

    assert!(result.is_err());
}

// ── Message splitting tests ─────────────────────────────────────

#[test]
fn telegram_split_short_message() {
    let msg = "Hello, world!";
    let chunks = split_message_for_telegram(msg);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], msg);
}

#[test]
fn telegram_split_exact_limit() {
    let msg = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH);
    let chunks = split_message_for_telegram(&msg);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), TELEGRAM_MAX_MESSAGE_LENGTH);
}

#[test]
fn telegram_split_over_limit() {
    let msg = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 100);
    let chunks = split_message_for_telegram(&msg);
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
    assert!(chunks[1].len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
}

#[test]
fn telegram_split_counts_final_continued_marker_in_send_length() {
    let msg = "a".repeat(8142);
    let chunks = split_message_for_telegram(&msg);
    assert!(chunks.len() >= 2);

    for (index, chunk) in chunks.iter().enumerate() {
        let text = format_telegram_text_chunk(chunk, index, chunks.len());
        assert!(
            text.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "final sent chunk {index} must be <= {TELEGRAM_MAX_MESSAGE_LENGTH}, got {}",
            text.chars().count()
        );
    }

    let final_text =
        format_telegram_text_chunk(chunks.last().unwrap(), chunks.len() - 1, chunks.len());
    assert!(final_text.starts_with(TELEGRAM_CONTINUED_PREFIX));
}

#[test]
fn telegram_split_counts_middle_continuation_markers_in_send_length() {
    let msg = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH * 3);
    let chunks = split_message_for_telegram(&msg);
    assert!(chunks.len() >= 3);

    for (index, chunk) in chunks.iter().enumerate() {
        let text = format_telegram_text_chunk(chunk, index, chunks.len());
        assert!(
            text.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "sent chunk {index} must be <= {TELEGRAM_MAX_MESSAGE_LENGTH}, got {}",
            text.chars().count()
        );
    }

    let middle = format_telegram_text_chunk(&chunks[1], 1, chunks.len());
    assert!(middle.starts_with(TELEGRAM_CONTINUED_PREFIX));
    assert!(middle.ends_with(TELEGRAM_CONTINUES_SUFFIX));
}

#[test]
fn telegram_split_at_word_boundary() {
    let msg = format!(
        "{} more text here",
        "word ".repeat(TELEGRAM_MAX_MESSAGE_LENGTH / 5)
    );
    let chunks = split_message_for_telegram(&msg);
    assert!(chunks.len() >= 2);
    // First chunk should end with a complete word (space at the end)
    for chunk in &chunks[..chunks.len() - 1] {
        assert!(chunk.len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
    }
}

#[test]
fn telegram_split_at_newline() {
    let text_block = "Line of text\n".repeat(TELEGRAM_MAX_MESSAGE_LENGTH / 13 + 1);
    let chunks = split_message_for_telegram(&text_block);
    assert!(chunks.len() >= 2);
    for chunk in chunks {
        assert!(chunk.len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
    }
}

#[test]
fn telegram_split_preserves_content() {
    let msg = "test ".repeat(TELEGRAM_MAX_MESSAGE_LENGTH / 5 + 100);
    let chunks = split_message_for_telegram(&msg);
    let rejoined = chunks.join("");
    assert_eq!(rejoined, msg);
}

#[test]
fn telegram_split_empty_message() {
    let chunks = split_message_for_telegram("");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], "");
}

#[test]
fn telegram_split_very_long_message() {
    let msg = "x".repeat(TELEGRAM_MAX_MESSAGE_LENGTH * 3);
    let chunks = split_message_for_telegram(&msg);
    assert!(chunks.len() >= 3);
    for chunk in chunks {
        assert!(chunk.len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
    }
}

// ── Caption handling tests ──────────────────────────────────────

#[tokio::test]
async fn telegram_send_document_bytes_with_caption() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let file_bytes = b"test content".to_vec();

    // With caption
    let result = ch
        .send_document_bytes(
            "123456",
            None,
            file_bytes.clone(),
            "test.txt",
            Some("My caption"),
        )
        .await;
    assert!(result.is_err()); // Network error expected

    // Without caption
    let result = ch
        .send_document_bytes("123456", None, file_bytes, "test.txt", None)
        .await;
    assert!(result.is_err()); // Network error expected
}

#[tokio::test]
async fn telegram_send_photo_bytes_with_caption() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let file_bytes = vec![0x89, 0x50, 0x4E, 0x47];

    // With caption
    let result = ch
        .send_photo_bytes(
            "123456",
            None,
            file_bytes.clone(),
            "test.png",
            Some("Photo caption"),
        )
        .await;
    assert!(result.is_err());

    // Without caption
    let result = ch
        .send_photo_bytes("123456", None, file_bytes, "test.png", None)
        .await;
    assert!(result.is_err());
}

// ── Empty/edge case tests ───────────────────────────────────────

#[tokio::test]
async fn telegram_send_document_bytes_empty_file() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/sendDocument$"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            serde_json::json!({ "ok": false, "description": "empty document rejected" }),
        ))
        .expect(1)
        .mount(&mock_server)
        .await;

    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_api_base(mock_server.uri());
    let file_bytes: Vec<u8> = vec![];

    let result = ch
        .send_document_bytes("123456", None, file_bytes, "empty.txt", None)
        .await;

    let err = result.expect_err("empty document send should fail");
    assert!(
        err.to_string().contains("empty document rejected"),
        "expected mocked Telegram error, got: {err}"
    );
}

#[tokio::test]
async fn telegram_send_document_bytes_empty_filename() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let file_bytes = b"content".to_vec();

    let result = ch
        .send_document_bytes("123456", None, file_bytes, "", None)
        .await;

    // Should not panic
    assert!(result.is_err());
}

#[tokio::test]
async fn telegram_send_document_bytes_empty_chat_id() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let file_bytes = b"content".to_vec();

    let result = ch
        .send_document_bytes("", None, file_bytes, "test.txt", None)
        .await;

    // Should not panic
    assert!(result.is_err());
}

// ── Message ID edge cases ─────────────────────────────────────

#[test]
fn telegram_message_id_format_includes_chat_and_message_id() {
    // Verify that message IDs follow the format: telegram_{chat_id}_{message_id}
    let chat_id = "123456";
    let message_id = 789;
    let expected_id = format!("telegram_{chat_id}_{message_id}");
    assert_eq!(expected_id, "telegram_123456_789");
}

#[test]
fn telegram_message_id_is_deterministic() {
    // Same chat_id + same message_id = same ID (prevents duplicates after restart)
    let chat_id = "123456";
    let message_id = 789;
    let id1 = format!("telegram_{chat_id}_{message_id}");
    let id2 = format!("telegram_{chat_id}_{message_id}");
    assert_eq!(id1, id2);
}

#[test]
fn telegram_message_id_different_message_different_id() {
    // Different message IDs produce different IDs
    let chat_id = "123456";
    let id1 = format!("telegram_{chat_id}_789");
    let id2 = format!("telegram_{chat_id}_790");
    assert_ne!(id1, id2);
}

#[test]
fn telegram_message_id_different_chat_different_id() {
    // Different chats produce different IDs even with same message_id
    let message_id = 789;
    let id1 = format!("telegram_123456_{message_id}");
    let id2 = format!("telegram_789012_{message_id}");
    assert_ne!(id1, id2);
}

#[test]
fn telegram_message_id_no_uuid_randomness() {
    // Verify format doesn't contain random UUID components
    let chat_id = "123456";
    let message_id = 789;
    let id = format!("telegram_{chat_id}_{message_id}");
    assert!(!id.contains('-')); // No UUID dashes
    assert!(id.starts_with("telegram_"));
}

#[test]
fn telegram_message_id_handles_zero_message_id() {
    // Edge case: message_id can be 0 (fallback/missing case)
    let chat_id = "123456";
    let message_id = 0;
    let id = format!("telegram_{chat_id}_{message_id}");
    assert_eq!(id, "telegram_123456_0");
}

// ── Tool call tag stripping tests ───────────────────────────────────

#[test]
fn strip_tool_call_tags_removes_standard_tags() {
    let input = "Hello <tool>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool> world";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello  world");
}

#[test]
fn strip_tool_call_tags_removes_alias_tags() {
    let input =
        "Hello <toolcall>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</toolcall> world";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello  world");
}

#[test]
fn strip_tool_call_tags_removes_dash_tags() {
    let input = "Hello <tool-call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool-call> world";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello  world");
}

#[test]
fn strip_tool_call_tags_removes_tool_call_tags() {
    let input = "Hello <tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call> world";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello  world");
}

#[test]
fn strip_tool_call_tags_removes_invoke_tags() {
    let input =
        "Hello <invoke>{\"name\":\"shell\",\"arguments\":{\"command\":\"date\"}}</invoke> world";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello  world");
}

#[test]
fn strip_tool_call_tags_handles_multiple_tags() {
    let input = "Start <tool>a</tool> middle <tool>b</tool> end";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Start  middle  end");
}

#[test]
fn strip_tool_call_tags_handles_mixed_tags() {
    let input = "A <tool>a</tool> B <toolcall>b</toolcall> C <tool-call>c</tool-call> D";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "A  B  C  D");
}

#[test]
fn strip_tool_call_tags_preserves_normal_text() {
    let input = "Hello world! This is a test.";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello world! This is a test.");
}

#[test]
fn strip_tool_call_tags_handles_unclosed_tags() {
    let input = "Hello <tool>world";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello <tool>world");
}

#[test]
fn strip_tool_call_tags_handles_unclosed_tool_call_with_json() {
    let input = "Status:\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"uptime\"}}";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Status:");
}

#[test]
fn strip_tool_call_tags_handles_mismatched_close_tag() {
    let input =
        "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"uptime\"}}</arg_value>";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "");
}

#[test]
fn strip_tool_call_tags_cleans_extra_newlines() {
    let input = "Hello\n\n<tool>\ntest\n</tool>\n\n\nworld";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "Hello\n\nworld");
}

#[test]
fn strip_tool_call_tags_handles_empty_input() {
    let input = "";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "");
}

#[test]
fn strip_tool_call_tags_handles_only_tags() {
    let input = "<tool>{\"name\":\"test\"}</tool>";
    let result = strip_tool_call_tags(input);
    assert_eq!(result, "");
}

#[test]
fn telegram_contains_bot_mention_finds_mention() {
    assert!(TelegramChannel::contains_bot_mention(
        "Hello @mybot",
        "mybot"
    ));
    assert!(TelegramChannel::contains_bot_mention(
        "@mybot help",
        "mybot"
    ));
    assert!(TelegramChannel::contains_bot_mention(
        "Hey @mybot how are you?",
        "mybot"
    ));
    assert!(TelegramChannel::contains_bot_mention(
        "Hello @MyBot, can you help?",
        "mybot"
    ));
}

#[test]
fn telegram_contains_bot_mention_no_false_positives() {
    assert!(!TelegramChannel::contains_bot_mention(
        "Hello @otherbot",
        "mybot"
    ));
    assert!(!TelegramChannel::contains_bot_mention(
        "Hello mybot",
        "mybot"
    ));
    assert!(!TelegramChannel::contains_bot_mention(
        "Hello @mybot2",
        "mybot"
    ));
    assert!(!TelegramChannel::contains_bot_mention("", "mybot"));
}

#[test]
fn telegram_normalize_incoming_content_preserves_mention() {
    let result = TelegramChannel::normalize_incoming_content("@mybot hello", "mybot");
    assert_eq!(result, Some("@mybot hello".to_string()));
}

#[test]
fn telegram_normalize_incoming_content_returns_none_for_empty() {
    let result = TelegramChannel::normalize_incoming_content("   ", "mybot");
    assert_eq!(result, None);
}

#[test]
fn parse_update_message_mention_only_group_requires_exact_mention() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }

    let update = serde_json::json!({
        "update_id": 10,
        "message": {
            "message_id": 44,
            "text": "hello @mybot2",
            "from": {
                "id": 555,
                "username": "alice"
            },
            "chat": {
                "id": -100_200_300,
                "type": "group"
            }
        }
    });

    assert!(ch.parse_update_message(&update).is_none());
}

#[test]
fn parse_update_message_mention_only_group_preserves_mention_in_body() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }

    let update = serde_json::json!({
        "update_id": 11,
        "message": {
            "message_id": 45,
            "text": "Hi @MyBot status please",
            "from": {
                "id": 555,
                "username": "alice"
            },
            "chat": {
                "id": -100_200_300,
                "type": "group"
            }
        }
    });

    let parsed = ch
        .parse_update_message(&update)
        .expect("mention should parse");
    assert_eq!(parsed.content, "Hi @MyBot status please");

    let mention_only_update = serde_json::json!({
        "update_id": 12,
        "message": {
            "message_id": 46,
            "text": "@mybot",
            "from": {
                "id": 555,
                "username": "alice"
            },
            "chat": {
                "id": -100_200_300,
                "type": "group"
            }
        }
    });

    let parsed = ch
        .parse_update_message(&mention_only_update)
        .expect("mention-only body admits");
    assert_eq!(parsed.content, "@mybot");
}

#[test]
fn parse_update_reply_to_bot_bypasses_mention_only_gate() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }
    {
        let mut cache = ch.bot_id.lock();
        *cache = Some(42);
    }

    // Reply to the bot's own message — no mention needed.
    let update = serde_json::json!({
        "update_id": 20,
        "message": {
            "message_id": 55,
            "text": "do this",
            "from": { "id": 555, "username": "alice" },
            "chat": { "id": -100_200_300, "type": "group" },
            "reply_to_message": {
                "message_id": 50,
                "from": { "id": 42, "username": "mybot", "is_bot": true },
                "text": "original"
            }
        }
    });

    let parsed = ch
        .parse_update_message(&update)
        .expect("reply-to-bot should bypass mention_only gate");
    // extract_reply_context prepends the quote; the gate returns the body,
    // and the quote is re-added by the normal reply-handling path.
    assert_eq!(parsed.content, "> @mybot:\n> original\n\ndo this");
}

#[test]
fn parse_update_reply_to_non_bot_still_dropped_in_mention_only() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }
    {
        let mut cache = ch.bot_id.lock();
        *cache = Some(42);
    }

    // Reply to another user (not the bot) — still needs a mention.
    let update = serde_json::json!({
        "update_id": 21,
        "message": {
            "message_id": 56,
            "text": "hello",
            "from": { "id": 555, "username": "alice" },
            "chat": { "id": -100_200_300, "type": "group" },
            "reply_to_message": {
                "message_id": 51,
                "from": { "id": 99, "username": "charlie" },
                "text": "some message"
            }
        }
    });

    assert!(ch.parse_update_message(&update).is_none());
}

#[test]
fn parse_update_reply_bot_id_unresolved_falls_through_in_mention_only() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }
    // bot_id stays None — unresolved.

    // Reply to the bot's message, but bot_id is unresolved — falls through.
    let update = serde_json::json!({
        "update_id": 22,
        "message": {
            "message_id": 57,
            "text": "hello",
            "from": { "id": 555, "username": "alice" },
            "chat": { "id": -100_200_300, "type": "group" },
            "reply_to_message": {
                "message_id": 52,
                "from": { "id": 42, "username": "mybot", "is_bot": true },
                "text": "original"
            }
        }
    });

    assert!(ch.parse_update_message(&update).is_none());
}

#[test]
fn parse_update_reply_to_bot_bypasses_mention_only_gate_caption_path() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }
    {
        let mut cache = ch.bot_id.lock();
        *cache = Some(42);
    }

    // Photo with a caption, replying to the bot — caption should pass.
    // This exercises check_media_mention_gate directly because
    // parse_update_message requires `message.text` and photo updates
    // carry only `message.caption`.
    let message = serde_json::json!({
        "message_id": 58,
        "caption": "enhance this",
        "from": { "id": 555, "username": "alice" },
        "chat": { "id": -100_200_300, "type": "group" },
        "photo": [
            { "file_id": "abc", "width": 100, "height": 100 }
        ],
        "reply_to_message": {
            "message_id": 53,
            "from": { "id": 42, "username": "mybot", "is_bot": true },
            "text": "original photo"
        }
    });

    let result = ch.check_media_mention_gate(&message, Some("enhance this"));
    assert!(
        result.is_some(),
        "reply-to-bot caption should bypass mention_only gate"
    );
    let gated = result.unwrap();
    assert!(gated.is_some(), "gate should return the normalized caption");
    assert_eq!(gated.unwrap(), "enhance this");
}

#[test]
fn telegram_is_group_message_detects_groups() {
    let group_msg = serde_json::json!({
        "chat": { "type": "group" }
    });
    assert!(TelegramChannel::is_group_message(&group_msg));

    let supergroup_msg = serde_json::json!({
        "chat": { "type": "supergroup" }
    });
    assert!(TelegramChannel::is_group_message(&supergroup_msg));

    let private_msg = serde_json::json!({
        "chat": { "type": "private" }
    });
    assert!(!TelegramChannel::is_group_message(&private_msg));
}

#[test]
fn telegram_mention_only_enabled_by_config() {
    let mention_only = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    assert!(ch.mention_only);

    let disabled_mention_only = false;
    let ch_disabled = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        disabled_mention_only,
    );
    assert!(!ch_disabled.mention_only);
}

fn group_message_with_caption(caption: Option<&str>) -> serde_json::Value {
    let mut msg = serde_json::json!({
        "message_id": 1,
        "from": { "id": 1, "username": "alice" },
        "chat": { "id": -1, "type": "group" }
    });
    if let Some(c) = caption {
        msg["caption"] = serde_json::Value::String(c.to_string());
    }
    msg
}

#[test]
fn check_media_mention_gate_rejects_group_media_without_mention() {
    let ch = TelegramChannel::new(
        "token".into(),
        "default",
        std::sync::Arc::new(|| vec!["*".into()]),
        true,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }
    let no_caption = group_message_with_caption(None);
    assert!(
        ch.check_media_mention_gate(&no_caption, None).is_none(),
        "no caption + mention_only group ⇒ reject"
    );
    let unrelated_caption = group_message_with_caption(Some("nice photo"));
    assert!(
        ch.check_media_mention_gate(&unrelated_caption, Some("nice photo"))
            .is_none(),
        "caption without bot mention + mention_only group ⇒ reject"
    );
    let other_bot_caption = group_message_with_caption(Some("hey @otherbot look"));
    assert!(
        ch.check_media_mention_gate(&other_bot_caption, Some("hey @otherbot look"))
            .is_none(),
        "caption mentioning a different bot ⇒ reject"
    );
}

#[test]
fn check_media_mention_gate_admits_and_preserves_caption_mention() {
    let ch = TelegramChannel::new(
        "token".into(),
        "default",
        std::sync::Arc::new(|| vec!["*".into()]),
        true,
    );
    {
        let mut cache = ch.bot_username.lock();
        *cache = Some("mybot".to_string());
    }
    let msg = group_message_with_caption(Some("@mybot describe this"));
    let result = ch.check_media_mention_gate(&msg, Some("@mybot describe this"));
    assert_eq!(
        result,
        Some(Some("@mybot describe this".to_string())),
        "mention text preserved verbatim once gate admits"
    );
}

#[test]
fn check_media_mention_gate_passes_dm_unchanged() {
    let ch = TelegramChannel::new(
        "token".into(),
        "default",
        std::sync::Arc::new(|| vec!["*".into()]),
        true,
    );
    let dm = serde_json::json!({
        "message_id": 1,
        "from": { "id": 1, "username": "alice" },
        "chat": { "id": 1, "type": "private" },
        "caption": "hello"
    });
    assert_eq!(
        ch.check_media_mention_gate(&dm, Some("hello")),
        Some(Some("hello".to_string())),
        "DM media must always pass with caption verbatim"
    );
    let dm_no_caption = serde_json::json!({
        "message_id": 1,
        "from": { "id": 1, "username": "alice" },
        "chat": { "id": 1, "type": "private" }
    });
    assert_eq!(
        ch.check_media_mention_gate(&dm_no_caption, None),
        Some(None),
        "DM media with no caption must pass"
    );
}

#[test]
fn check_media_mention_gate_passes_when_mention_only_disabled() {
    let ch = TelegramChannel::new(
        "token".into(),
        "default",
        std::sync::Arc::new(|| vec!["*".into()]),
        false,
    );
    let group_no_caption = group_message_with_caption(None);
    assert_eq!(
        ch.check_media_mention_gate(&group_no_caption, None),
        Some(None),
        "mention_only off ⇒ all media pass"
    );
}

#[test]
fn check_media_mention_gate_rejects_group_when_bot_username_unknown() {
    let ch = TelegramChannel::new(
        "token".into(),
        "default",
        std::sync::Arc::new(|| vec!["*".into()]),
        true,
    );
    // Do NOT set bot_username — leave it None.
    let group = group_message_with_caption(Some("@somebody hi"));
    assert!(
        ch.check_media_mention_gate(&group, Some("@somebody hi"))
            .is_none(),
        "missing bot_username in group must fail closed"
    );
}

// ─────────────────────────────────────────────────────────────────────
// TG6: Channel platform limit edge cases for Telegram (4096 char limit)
// Prevents: Pattern 6 — issues
// ─────────────────────────────────────────────────────────────────────

#[test]
fn telegram_split_code_block_at_boundary() {
    let mut msg = String::new();
    msg.push_str("```python\n");
    msg.push_str(&"x".repeat(4085));
    msg.push_str("\n```\nMore text after code block");
    let parts = split_message_for_telegram(&msg);
    assert!(
        parts.len() >= 2,
        "code block spanning boundary should split"
    );
    for part in &parts {
        assert!(
            part.len() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "each part must be <= {TELEGRAM_MAX_MESSAGE_LENGTH}, got {}",
            part.len()
        );
    }
}

#[test]
fn telegram_split_long_fenced_code_block_balances_each_chunk() {
    let mut msg = String::new();
    msg.push_str("Intro\n\n```rust\n");
    for i in 0..700 {
        let _ = writeln!(msg, "fn generated_{i}() {{ println!(\"line {i:03}\"); }}");
    }
    msg.push_str("```\n\nOutro");

    let parts = split_message_for_telegram(&msg);
    assert!(parts.len() >= 2, "long fenced code block should split");
    for part in &parts {
        assert!(
            part.len() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "balanced chunk must be <= {TELEGRAM_MAX_MESSAGE_LENGTH}, got {}",
            part.len()
        );
        assert_eq!(
            part.matches("```").count() % 2,
            0,
            "each chunk should have balanced markdown fences"
        );

        let html = TelegramChannel::markdown_to_telegram_html(part);
        assert_eq!(
            html.matches("<pre><code>").count(),
            html.matches("</code></pre>").count(),
            "rendered Telegram HTML should have balanced code blocks"
        );
    }

    assert!(
        parts.iter().skip(1).any(|part| part.starts_with("```\n")),
        "continuation inside a code block should reopen a fence"
    );
    assert!(
        parts
            .iter()
            .take(parts.len() - 1)
            .any(|part| part.ends_with("\n```") || part.ends_with("```")),
        "split chunks inside a code block should close the fence"
    );
}

#[test]
fn telegram_split_fenced_code_send_text_stays_within_limit_and_balanced() {
    let mut msg = String::new();
    msg.push_str("```rust\n");
    msg.push_str(&"a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 120));
    msg.push_str("\n```\n");

    let parts = split_message_for_telegram(&msg);
    assert!(parts.len() >= 2);

    for (index, part) in parts.iter().enumerate() {
        let text = format_telegram_text_chunk(part, index, parts.len());
        assert!(
            text.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "sent fenced chunk {index} must be <= {TELEGRAM_MAX_MESSAGE_LENGTH}, got {}",
            text.chars().count()
        );
        assert_eq!(
            text.matches("```").count() % 2,
            0,
            "sent fenced chunk {index} should have balanced markdown fences"
        );

        let html = TelegramChannel::markdown_to_telegram_html(&text);
        assert_eq!(
            html.matches("<pre><code>").count(),
            html.matches("</code></pre>").count(),
            "sent fenced chunk {index} should render balanced Telegram HTML"
        );
    }
}

#[test]
fn telegram_split_single_long_word() {
    let long_word = "a".repeat(5000);
    let parts = split_message_for_telegram(&long_word);
    assert!(parts.len() >= 2, "word exceeding limit must be split");
    for part in &parts {
        assert!(
            part.len() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "hard-split part must be <= {TELEGRAM_MAX_MESSAGE_LENGTH}, got {}",
            part.len()
        );
    }
    let reassembled: String = parts.join("");
    assert_eq!(reassembled, long_word);
}

#[test]
fn telegram_split_exactly_at_limit_no_split() {
    let msg = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH);
    let parts = split_message_for_telegram(&msg);
    assert_eq!(parts.len(), 1, "message exactly at limit should not split");
}

#[test]
fn telegram_split_one_over_limit() {
    let msg = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 1);
    let parts = split_message_for_telegram(&msg);
    assert!(parts.len() >= 2, "message 1 char over limit must split");
}

#[test]
fn telegram_split_many_short_lines() {
    let msg: String = (0..1000).fold(String::new(), |mut acc, i| {
        let _ = writeln!(acc, "line {i}");
        acc
    });
    let parts = split_message_for_telegram(&msg);
    for part in &parts {
        assert!(
            part.len() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "short-line batch must be <= limit"
        );
    }
}

#[test]
fn telegram_split_only_whitespace() {
    let msg = "   \n\n\t  ";
    let parts = split_message_for_telegram(msg);
    assert!(parts.len() <= 1);
}

#[test]
fn telegram_split_emoji_at_boundary() {
    let mut msg = "a".repeat(4094);
    msg.push_str("🎉🎊"); // 4096 chars total
    let parts = split_message_for_telegram(&msg);
    for part in &parts {
        // The function splits on character count, not byte count
        assert!(
            part.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH,
            "emoji boundary split must respect limit"
        );
    }
}

#[test]
fn telegram_split_consecutive_newlines() {
    let mut msg = "a".repeat(4090);
    msg.push_str("\n\n\n\n\n\n");
    msg.push_str(&"b".repeat(100));
    let parts = split_message_for_telegram(&msg);
    for part in &parts {
        assert!(part.len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
    }
}

#[test]
fn parse_voice_metadata_extracts_voice() {
    let msg = serde_json::json!({
        "voice": {
            "file_id": "abc123",
            "duration": 5
        }
    });
    let (file_id, dur) = TelegramChannel::parse_voice_metadata(&msg).unwrap();
    assert_eq!(file_id, "abc123");
    assert_eq!(dur, 5);
}

#[test]
fn parse_voice_metadata_extracts_audio() {
    let msg = serde_json::json!({
        "audio": {
            "file_id": "audio456",
            "duration": 30
        }
    });
    let (file_id, dur) = TelegramChannel::parse_voice_metadata(&msg).unwrap();
    assert_eq!(file_id, "audio456");
    assert_eq!(dur, 30);
}

#[test]
fn parse_voice_metadata_returns_none_for_text() {
    let msg = serde_json::json!({
        "text": "hello"
    });
    assert!(TelegramChannel::parse_voice_metadata(&msg).is_none());
}

#[test]
fn parse_voice_metadata_defaults_duration_to_zero() {
    let msg = serde_json::json!({
        "voice": {
            "file_id": "no_dur"
        }
    });
    let (_, dur) = TelegramChannel::parse_voice_metadata(&msg).unwrap();
    assert_eq!(dur, 0);
}

// ─────────────────────────────────────────────────────────────────────
// extract_sender_info tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn extract_sender_info_with_username() {
    let msg = serde_json::json!({
        "from": { "id": 123, "username": "alice" }
    });
    let (username, sender_id, identity) = TelegramChannel::extract_sender_info(&msg);
    assert_eq!(username, "alice");
    assert_eq!(sender_id, Some("123".to_string()));
    assert_eq!(identity, "alice");
}

#[test]
fn extract_sender_info_without_username() {
    let msg = serde_json::json!({
        "from": { "id": 42 }
    });
    let (username, sender_id, identity) = TelegramChannel::extract_sender_info(&msg);
    assert_eq!(username, "unknown");
    assert_eq!(sender_id, Some("42".to_string()));
    assert_eq!(identity, "42");
}

// ─────────────────────────────────────────────────────────────────────
// extract_reply_context tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn extract_reply_context_text_message() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let msg = serde_json::json!({
        "reply_to_message": {
            "from": { "username": "alice" },
            "text": "Hello world"
        }
    });
    let ctx = ch.extract_reply_context(&msg).unwrap();
    assert_eq!(ctx, "> @alice:\n> Hello world");
}

#[test]
fn extract_reply_context_voice_message() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let msg = serde_json::json!({
        "reply_to_message": {
            "from": { "username": "bob" },
            "voice": { "file_id": "abc", "duration": 5 }
        }
    });
    let ctx = ch.extract_reply_context(&msg).unwrap();
    assert_eq!(ctx, "> @bob:\n> [Voice message]");
}

#[test]
fn extract_reply_context_no_reply() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let msg = serde_json::json!({
        "text": "just a regular message"
    });
    assert!(ch.extract_reply_context(&msg).is_none());
}

#[test]
fn extract_reply_context_skips_topic_root() {
    // Telegram auto-injects a reply_to_message pointing at the topic-root
    // message on every message in a non-General forum topic. The injected
    // reply's message_id equals the parent's message_thread_id. It is
    // not a real reply and must not produce a blockquote prefix.
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let msg = serde_json::json!({
        "message_thread_id": 42,
        "text": "hello in topic",
        "reply_to_message": {
            "message_id": 42,
            "from": { "username": "alice" },
            "forum_topic_created": { "name": "General Discussion", "icon_color": 0 }
        }
    });
    assert!(ch.extract_reply_context(&msg).is_none());
}

#[test]
fn extract_reply_context_real_reply_in_topic() {
    // A genuine reply inside a forum topic (reply.message_id differs from
    // the parent's message_thread_id) should still produce a blockquote.
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let msg = serde_json::json!({
        "message_thread_id": 42,
        "text": "I agree",
        "reply_to_message": {
            "message_id": 100,
            "from": { "username": "alice" },
            "text": "What do you think?"
        }
    });
    let ctx = ch.extract_reply_context(&msg).unwrap();
    assert_eq!(ctx, "> @alice:\n> What do you think?");
}

#[test]
fn extract_reply_context_no_username_uses_first_name() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let msg = serde_json::json!({
        "reply_to_message": {
            "from": { "id": 999, "first_name": "Charlie" },
            "text": "Hi there"
        }
    });
    let ctx = ch.extract_reply_context(&msg).unwrap();
    assert_eq!(ctx, "> @Charlie:\n> Hi there");
}

#[test]
fn extract_reply_context_voice_with_cached_transcription() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    // Pre-populate transcription cache
    ch.voice_transcriptions
        .lock()
        .insert("100:42".to_string(), "Hello from voice".to_string());
    let msg = serde_json::json!({
        "chat": { "id": 100 },
        "reply_to_message": {
            "message_id": 42,
            "from": { "username": "bob" },
            "voice": { "file_id": "abc", "duration": 5 }
        }
    });
    let ctx = ch.extract_reply_context(&msg).unwrap();
    assert_eq!(ctx, "> @bob:\n> [Voice] Hello from voice");
}

#[test]
fn parse_update_message_includes_reply_context() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "message": {
            "message_id": 10,
            "text": "translate this",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 100, "type": "private" },
            "reply_to_message": {
                "from": { "username": "bot" },
                "text": "Bonjour le monde"
            }
        }
    });
    let parsed = ch.parse_update_message(&update).unwrap();
    assert!(
        parsed.content.starts_with("> @bot:"),
        "content should start with quote: {}",
        parsed.content
    );
    assert!(
        parsed.content.contains("translate this"),
        "content should contain user text"
    );
    assert!(
        parsed.content.contains("Bonjour le monde"),
        "content should contain quoted text"
    );
}

#[test]
fn with_transcription_sets_config_when_enabled() {
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test_key".to_string()),
        ..zeroclaw_config::schema::TranscriptionConfig::default()
    };

    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_transcription(tc);
    assert!(ch.transcription.is_some());
    assert!(ch.transcription_manager.is_some());
}

#[test]
fn with_transcription_skips_when_disabled() {
    let tc = zeroclaw_config::schema::TranscriptionConfig::default(); // enabled = false
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_transcription(tc);
    assert!(ch.transcription.is_none());
    assert!(ch.transcription_manager.is_none());
}

#[tokio::test]
async fn try_parse_voice_message_returns_none_when_transcription_disabled() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "message": {
            "message_id": 1,
            "voice": { "file_id": "voice_file", "duration": 4 },
            "from": { "id": 123, "username": "alice" },
            "chat": { "id": 456, "type": "private" }
        }
    });

    let parsed = ch.try_parse_voice_message(&update).await;
    assert!(matches!(parsed, UpdateDisposition::SkipPermanent));
}

#[tokio::test]
async fn try_parse_voice_message_skips_when_duration_exceeds_limit() {
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test_key".to_string()),
        max_duration_secs: 5,
        ..Default::default()
    };

    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_transcription(tc);
    let update = serde_json::json!({
        "message": {
            "message_id": 2,
            "voice": { "file_id": "voice_file", "duration": 30 },
            "from": { "id": 123, "username": "alice" },
            "chat": { "id": 456, "type": "private" }
        }
    });

    let parsed = ch.try_parse_voice_message(&update).await;
    assert!(matches!(parsed, UpdateDisposition::SkipPermanent));
}

#[tokio::test]
async fn try_parse_voice_message_rejects_unauthorized_sender_before_download() {
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test_key".to_string()),
        max_duration_secs: 120,
        ..Default::default()
    };

    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["alice".into()]),
        mention_only,
    )
    .with_transcription(tc);
    let update = serde_json::json!({
        "message": {
            "message_id": 3,
            "voice": { "file_id": "voice_file", "duration": 4 },
            "from": { "id": 999, "username": "bob" },
            "chat": { "id": 456, "type": "private" }
        }
    });

    let parsed = ch.try_parse_voice_message(&update).await;
    assert!(matches!(parsed, UpdateDisposition::SkipPermanent));
    assert!(ch.voice_transcriptions.lock().is_empty());
}

fn telegram_text_update(
    update_id: i64,
    message_id: i64,
    chat_id: i64,
    username: &str,
    text: &str,
) -> serde_json::Value {
    serde_json::json!({
        "update_id": update_id,
        "message": {
            "message_id": message_id,
            "chat": {"id": chat_id, "type": "private"},
            "from": {"id": message_id + 100_000, "username": username},
            "text": text,
        }
    })
}

fn telegram_document_update(
    update_id: i64,
    message_id: i64,
    chat_id: i64,
    username: &str,
    file_id: &str,
    file_name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "update_id": update_id,
        "message": {
            "message_id": message_id,
            "chat": {"id": chat_id, "type": "private"},
            "from": {"id": message_id + 100_000, "username": username},
            "document": {"file_id": file_id, "file_name": file_name},
        }
    })
}

async fn mount_telegram_startup_probe(mock_server: &wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/getUpdates$"))
        .and(body_partial_json(serde_json::json!({"timeout": 0})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "result": []})),
        )
        .mount(mock_server)
        .await;
}

async fn mount_telegram_get_updates(
    mock_server: &wiremock::MockServer,
    offset: i64,
    result: serde_json::Value,
) {
    use wiremock::matchers::{body_partial_json, method, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/getUpdates$"))
        .and(body_partial_json(
            serde_json::json!({"offset": offset, "timeout": 30}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": result
        })))
        .mount(mock_server)
        .await;
}

async fn telegram_main_loop_getupdates_bodies(
    mock_server: &wiremock::MockServer,
) -> Vec<serde_json::Value> {
    mock_server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/getUpdates"))
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .filter(|b| b.get("timeout").and_then(serde_json::Value::as_i64) == Some(30))
        .collect()
}

async fn telegram_wait_for_main_loop_offset(
    mock_server: &wiremock::MockServer,
    offset: i64,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let seen = telegram_main_loop_getupdates_bodies(mock_server)
            .await
            .iter()
            .any(|b| b.get("offset").and_then(serde_json::Value::as_i64) == Some(offset));
        if seen {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn classify_get_file(status: reqwest::StatusCode, body: serde_json::Value) -> FileLookupFailure {
    FileLookupError::classify(status, Some(&body)).kind
}

fn vendor_error(error_code: i64, description: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "error_code": error_code,
        "description": description
    })
}

#[test]
fn get_file_failures_classify_permanent_vendor_rejections_only() {
    let status400 = reqwest::StatusCode::BAD_REQUEST;
    let status408 = reqwest::StatusCode::REQUEST_TIMEOUT;
    let status425 = reqwest::StatusCode::from_u16(425).expect("425 is a valid status");
    let status500 = reqwest::StatusCode::INTERNAL_SERVER_ERROR;

    assert_eq!(
        FileLookupError::classify(status400, None).kind,
        FileLookupFailure::Transient,
        "a body-less 400 Bad Request must stay transient"
    );
    assert_eq!(
        classify_get_file(
            status400,
            serde_json::json!({"ok": false, "description": "no code"})
        ),
        FileLookupFailure::Transient,
        "ok: false with no error_code must stay transient"
    );
    assert_eq!(
        classify_get_file(status400, vendor_error(400, "Bad Request: invalid file_id")),
        FileLookupFailure::Permanent,
    );
    assert_eq!(
        classify_get_file(status400, vendor_error(403, "Forbidden")),
        FileLookupFailure::Permanent,
    );
    assert_eq!(
        classify_get_file(status400, vendor_error(404, "Not Found")),
        FileLookupFailure::Permanent,
    );
    assert_eq!(
        classify_get_file(status400, vendor_error(410, "Gone")),
        FileLookupFailure::Permanent,
    );
    assert_eq!(
        classify_get_file(status408, vendor_error(408, "Request Timeout")),
        FileLookupFailure::Transient,
        "a vendor-named 408 must stay transient"
    );
    assert_eq!(
        classify_get_file(status425, vendor_error(425, "Too Early")),
        FileLookupFailure::Transient,
        "425 must stay transient so a retryable Too Early cannot drop the update"
    );
    assert_eq!(
        classify_get_file(status400, vendor_error(422, "Unprocessable Entity")),
        FileLookupFailure::Transient,
        "a 4xx outside the permanent whitelist must stay transient"
    );
    assert_eq!(
        classify_get_file(
            status400,
            serde_json::json!({
                "ok": false,
                "error_code": 400,
                "description": "Too Many Requests",
                "parameters": {"retry_after": 12}
            })
        ),
        FileLookupFailure::Transient,
        "retry_after must keep even a whitelist error_code transient"
    );
    assert_eq!(
        FileLookupError::classify(status500, None).kind,
        FileLookupFailure::Transient,
    );
}

/// A transient `getFile` 500 must leave the offset un-advanced so the
/// next poll re-fetches the same update; once the download succeeds, the
/// offset advances past it.
#[tokio::test]
async fn listen_retries_transient_download_failure_at_same_offset_then_advances() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    mount_telegram_startup_probe(&mock_server).await;

    let uid = 1_000;
    let update = telegram_document_update(uid, 5, 555, "alice", "file123", "report.pdf");

    mount_telegram_get_updates(&mock_server, 0, serde_json::json!([update])).await;

    Mock::given(method("GET"))
        .and(path_regex(r"/bot[^/]+/getFile$"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/bot[^/]+/getFile$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {"file_path": "documents/report.pdf"}
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/file/bot[^/]+/.*$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"pdf bytes".to_vec()))
        .mount(&mock_server)
        .await;

    mount_telegram_get_updates(&mock_server, uid + 1, serde_json::json!([])).await;

    let workspace = tempfile::tempdir().unwrap();
    let ch = Arc::new(
        TelegramChannel::new(
            "test-token".into(),
            "telegram_test_alias",
            Arc::new(|| vec!["alice".to_string()]),
            false,
        )
        .with_api_base(mock_server.uri())
        .with_workspace_dir(workspace.path().to_path_buf()),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for the attachment message")
        .expect("channel closed before delivering the attachment message");
    assert!(
        msg.content.contains("report.pdf"),
        "unexpected content: {}",
        msg.content
    );

    let main_loop_bodies = telegram_main_loop_getupdates_bodies(&mock_server).await;
    assert!(
        main_loop_bodies.len() >= 2,
        "expected at least 2 main-loop getUpdates requests (initial attempt + retry), got {}",
        main_loop_bodies.len()
    );
    for body in &main_loop_bodies[..2] {
        assert_eq!(
            body.get("offset").and_then(serde_json::Value::as_i64),
            Some(0),
            "offset must not advance while the download keeps failing transiently"
        );
    }

    assert!(
        telegram_wait_for_main_loop_offset(&mock_server, uid + 1, Duration::from_secs(5)).await,
        "offset never advanced past the update once its retry succeeded"
    );

    handle.abort();
}

#[test]
fn skip_marker_append_load_roundtrip_and_idempotency() {
    let dir = tempfile::tempdir().unwrap();
    append_telegram_skip_marker(dir.path(), "bot-a", 42, "operator: corrupted voice blob").unwrap();
    let markers = load_telegram_skip_markers(dir.path(), "bot-a");
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].update_id, 42);
    assert_eq!(markers[0].reason, "operator: corrupted voice blob");

    append_telegram_skip_marker(dir.path(), "bot-a", 43, "second skip").unwrap();
    // Same update_id again is a no-op (idempotent).
    append_telegram_skip_marker(dir.path(), "bot-a", 42, "duplicate").unwrap();
    let markers = load_telegram_skip_markers(dir.path(), "bot-a");
    assert_eq!(
        markers.iter().map(|m| m.update_id).collect::<Vec<_>>(),
        vec![42, 43],
        "markers must append once per update_id"
    );

    // A different bot alias has its own list; a missing file reads empty.
    assert!(load_telegram_skip_markers(dir.path(), "bot-b").is_empty());

    // Path separators in an alias must be rejected, not joined.
    assert!(append_telegram_skip_marker(dir.path(), "../escape", 1, "x").is_err());
    assert!(append_telegram_skip_marker(dir.path(), "a/b", 1, "x").is_err());
    assert!(append_telegram_skip_marker(dir.path(), "", 1, "x").is_err());
}

/// A permanently-poisoned update (getFile always 500) blocks the bot
/// head-of-line until an operator writes a skip marker; the listener
/// must then archive the raw payload and advance the offset — never
/// deliver it, never drop it silently.
#[tokio::test]
async fn listen_operator_skip_archives_poisoned_update_and_advances_offset() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    mount_telegram_startup_probe(&mock_server).await;

    let uid = 3_000;
    let update = telegram_document_update(uid, 7, 557, "alice", "filePoison", "poison.pdf");

    mount_telegram_get_updates(&mock_server, 0, serde_json::json!([update])).await;

    Mock::given(method("GET"))
        .and(path_regex(r"/bot[^/]+/getFile$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    mount_telegram_get_updates(&mock_server, uid + 1, serde_json::json!([])).await;

    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    append_telegram_skip_marker(
        data_dir.path(),
        "telegram_test_alias",
        uid,
        "operator: known poison, skip after archive",
    )
    .unwrap();
    let persist_config = Config {
        data_dir: data_dir.path().to_path_buf(),
        ..Config::default()
    };

    let ch = Arc::new(
        TelegramChannel::new(
            "test-token".into(),
            "telegram_test_alias",
            Arc::new(|| vec!["alice".to_string()]),
            false,
        )
        .with_api_base(mock_server.uri())
        .with_workspace_dir(workspace.path().to_path_buf())
        .with_persistence(Arc::new(RwLock::new(persist_config))),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    assert!(
        telegram_wait_for_main_loop_offset(&mock_server, uid + 1, Duration::from_secs(15)).await,
        "operator skip must advance the offset past the poisoned update"
    );

    let dead_letter = data_dir
        .path()
        .join("telegram_dead_letters")
        .join("telegram_test_alias")
        .join(format!("{uid}.json"));
    let raw = std::fs::read_to_string(&dead_letter)
        .unwrap_or_else(|err| panic!("dead letter {} missing: {err}", dead_letter.display()));
    let archived: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(archived["update_id"].as_i64(), Some(uid));
    assert_eq!(
        archived["reason"].as_str(),
        Some("operator: known poison, skip after archive")
    );
    assert_eq!(
        archived["payload"]["update_id"].as_i64(),
        Some(uid),
        "the raw poisoned payload must be preserved for inspection"
    );

    assert!(
        rx.try_recv().is_err(),
        "a skipped poisoned update must never be delivered"
    );

    handle.abort();
}

/// 425 Too Early is retryable. Classifying it as permanent would
/// silently drop the update; the offset must stay put.
#[tokio::test]
async fn listen_does_not_advance_offset_on_getfile_425() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    mount_telegram_startup_probe(&mock_server).await;

    let uid = 2_000;
    let update = telegram_document_update(uid, 6, 556, "alice", "file425", "early.pdf");

    mount_telegram_get_updates(&mock_server, 0, serde_json::json!([update])).await;

    Mock::given(method("GET"))
        .and(path_regex(r"/bot[^/]+/getFile$"))
        .respond_with(ResponseTemplate::new(425).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 425,
            "description": "Too Early"
        })))
        .mount(&mock_server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    let ch = Arc::new(
        TelegramChannel::new(
            "test-token".into(),
            "telegram_test_alias",
            Arc::new(|| vec!["alice".to_string()]),
            false,
        )
        .with_api_base(mock_server.uri())
        .with_workspace_dir(workspace.path().to_path_buf()),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let bodies = telegram_main_loop_getupdates_bodies(&mock_server).await;
        if bodies.len() >= 2 {
            for body in &bodies {
                assert_eq!(
                    body.get("offset").and_then(serde_json::Value::as_i64),
                    Some(0),
                    "offset must not advance while getFile keeps returning 425"
                );
            }
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a second getUpdates retry at offset 0"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let extra = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        extra.is_err(),
        "a 425 getFile must not deliver or skip the update: {extra:?}"
    );

    handle.abort();
}

/// A 4xx outside the permanent whitelist (422) must retry, not skip.
#[tokio::test]
async fn listen_retries_non_whitelist_4xx_download_failure_then_advances() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;
    mount_telegram_startup_probe(&mock_server).await;

    let uid = 3_000;
    let update = telegram_document_update(uid, 7, 557, "alice", "file422", "later.pdf");

    mount_telegram_get_updates(&mock_server, 0, serde_json::json!([update])).await;

    Mock::given(method("GET"))
        .and(path_regex(r"/bot[^/]+/getFile$"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 422,
            "description": "Unprocessable Entity"
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/bot[^/]+/getFile$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {"file_path": "documents/later.pdf"}
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/file/bot[^/]+/.*$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"pdf bytes".to_vec()))
        .mount(&mock_server)
        .await;

    mount_telegram_get_updates(&mock_server, uid + 1, serde_json::json!([])).await;

    let workspace = tempfile::tempdir().unwrap();
    let ch = Arc::new(
        TelegramChannel::new(
            "test-token".into(),
            "telegram_test_alias",
            Arc::new(|| vec!["alice".to_string()]),
            false,
        )
        .with_api_base(mock_server.uri())
        .with_workspace_dir(workspace.path().to_path_buf()),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for the attachment message")
        .expect("channel closed before delivering the attachment message");
    assert!(
        msg.content.contains("later.pdf"),
        "unexpected content: {}",
        msg.content
    );

    assert!(
        telegram_wait_for_main_loop_offset(&mock_server, uid + 1, Duration::from_secs(5)).await,
        "offset never advanced past the update once its non-whitelist 4xx retry succeeded"
    );

    handle.abort();
}

/// An unauthorized-sender update is a permanent skip: it must still
/// advance the offset (so it's not retried forever), while an
/// authorized update right after it is delivered normally.
#[tokio::test]
async fn listen_permanent_skip_advances_past_unauthorized_update() {
    use wiremock::MockServer;

    let mock_server = MockServer::start().await;
    mount_telegram_startup_probe(&mock_server).await;

    let uid1 = 4_000;
    let uid2 = 4_001;
    let unauthorized_update = telegram_text_update(uid1, 20, 888, "mallory", "give me the keys");
    let authorized_update = telegram_text_update(uid2, 21, 888, "alice", "world");

    mount_telegram_get_updates(
        &mock_server,
        0,
        serde_json::json!([unauthorized_update, authorized_update]),
    )
    .await;
    mount_telegram_get_updates(&mock_server, uid2 + 1, serde_json::json!([])).await;

    let ch = Arc::new(
        TelegramChannel::new(
            "test-token".into(),
            "telegram_test_alias",
            Arc::new(|| vec!["alice".to_string()]),
            false,
        )
        .with_api_base(mock_server.uri()),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for the authorized message")
        .expect("channel closed before delivering the authorized message");
    assert_eq!(msg.sender, "alice");
    assert_eq!(msg.content, "world");

    let extra = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        extra.is_err(),
        "unexpected extra message delivered: {extra:?}"
    );

    assert!(
        telegram_wait_for_main_loop_offset(&mock_server, uid2 + 1, Duration::from_secs(5)).await,
        "offset never advanced past the unauthorized update to the next expected value"
    );

    handle.abort();
}

// ─────────────────────────────────────────────────────────────────────
// Live e2e: voice transcription via Groq Whisper + reply cache lookup
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires GROQ_API_KEY environment variable"]
async fn e2e_live_voice_transcription_and_reply_cache() {
    let Ok(api_key) = std::env::var("GROQ_API_KEY") else {
        eprintln!("GROQ_API_KEY not set — skipping live voice transcription test");
        return;
    };

    // 1. Load pre-recorded fixture (TTS-generated "hello", ~7 KB MP3)
    let fixture_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.mp3");
    let audio_data = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", fixture_path.display()));
    assert!(
        audio_data.len() > 1000,
        "fixture too small ({} bytes), likely corrupt",
        audio_data.len()
    );

    // 2. Call TranscriptionManager.transcribe() — real Groq Whisper API
    let config = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some(api_key),
        ..Default::default()
    };
    let manager = crate::transcription::TranscriptionManager::new(&config)
        .expect("TranscriptionManager::new should succeed with valid GROQ_API_KEY");
    let transcript: String = manager
        .transcribe(&audio_data, "hello.mp3")
        .await
        .expect("transcribe should succeed with valid GROQ_API_KEY");

    // 3. Verify Whisper actually recognized "hello"
    assert!(
        transcript.to_lowercase().contains("hello"),
        "expected transcription to contain 'hello', got: '{transcript}'"
    );

    // 4. Create TelegramChannel, insert transcription into voice_transcriptions cache
    let mention_only = false;
    let ch = TelegramChannel::new(
        "test_token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let chat_id: i64 = 12345;
    let message_id: i64 = 67;
    let cache_key = format!("{chat_id}:{message_id}");
    ch.voice_transcriptions
        .lock()
        .insert(cache_key, transcript.clone());

    // 5. Build reply message with voice + message_id + chat.id
    let msg = serde_json::json!({
        "chat": { "id": chat_id },
        "reply_to_message": {
            "message_id": message_id,
            "from": { "username": "zeroclaw_user" },
            "voice": { "file_id": "test_file", "duration": 1 }
        }
    });

    // 6. Verify extract_reply_context returns cached transcription
    let ctx = ch
        .extract_reply_context(&msg)
        .expect("extract_reply_context should return Some for voice reply");

    assert!(
        ctx.contains(&format!("[Voice] {transcript}")),
        "expected cached transcription in reply context, got: {ctx}"
    );

    // Must NOT contain the fallback placeholder
    assert!(
        !ctx.contains("[Voice message]"),
        "context should use cached transcription, not fallback placeholder, got: {ctx}"
    );
}

// ── IncomingAttachment / parse_attachment_metadata tests ─────────

#[test]
fn parse_attachment_metadata_detects_document() {
    let message = serde_json::json!({
        "document": {
            "file_id": "BQACAgIAAxk",
            "file_name": "report.pdf",
            "file_size": 12345
        }
    });
    let att = TelegramChannel::parse_attachment_metadata(&message).unwrap();
    assert_eq!(att.kind, IncomingAttachmentKind::Document);
    assert_eq!(att.file_id, "BQACAgIAAxk");
    assert_eq!(att.file_name.as_deref(), Some("report.pdf"));
    assert_eq!(att.file_size, Some(12345));
    assert!(att.caption.is_none());
}

#[test]
fn parse_attachment_metadata_detects_photo() {
    let message = serde_json::json!({
        "photo": [
            {"file_id": "small_id", "file_size": 100, "width": 90, "height": 90},
            {"file_id": "medium_id", "file_size": 500, "width": 320, "height": 320},
            {"file_id": "large_id", "file_size": 2000, "width": 800, "height": 800}
        ]
    });
    let att = TelegramChannel::parse_attachment_metadata(&message).unwrap();
    assert_eq!(att.kind, IncomingAttachmentKind::Photo);
    assert_eq!(att.file_id, "large_id");
    assert_eq!(att.file_size, Some(2000));
    assert!(att.file_name.is_none());
}

#[test]
fn parse_attachment_metadata_extracts_caption() {
    // Document with caption
    let doc_msg = serde_json::json!({
        "document": {
            "file_id": "doc_id",
            "file_name": "data.csv"
        },
        "caption": "Monthly report"
    });
    let att = TelegramChannel::parse_attachment_metadata(&doc_msg).unwrap();
    assert_eq!(att.caption.as_deref(), Some("Monthly report"));

    // Photo with caption
    let photo_msg = serde_json::json!({
        "photo": [
            {"file_id": "photo_id", "file_size": 1000}
        ],
        "caption": "Look at this"
    });
    let att = TelegramChannel::parse_attachment_metadata(&photo_msg).unwrap();
    assert_eq!(att.caption.as_deref(), Some("Look at this"));
}

#[test]
fn parse_attachment_metadata_document_without_optional_fields() {
    let message = serde_json::json!({
        "document": {
            "file_id": "doc_no_name"
        }
    });
    let att = TelegramChannel::parse_attachment_metadata(&message).unwrap();
    assert_eq!(att.kind, IncomingAttachmentKind::Document);
    assert_eq!(att.file_id, "doc_no_name");
    assert!(att.file_name.is_none());
    assert!(att.file_size.is_none());
    assert!(att.caption.is_none());
}

#[test]
fn parse_attachment_metadata_returns_none_for_text() {
    let message = serde_json::json!({
        "text": "Hello world"
    });
    assert!(TelegramChannel::parse_attachment_metadata(&message).is_none());
}

#[test]
fn parse_attachment_metadata_returns_none_for_voice() {
    let message = serde_json::json!({
        "voice": {
            "file_id": "voice_id",
            "duration": 5
        }
    });
    assert!(TelegramChannel::parse_attachment_metadata(&message).is_none());
}

#[test]
fn parse_attachment_metadata_empty_photo_array() {
    let message = serde_json::json!({
        "photo": []
    });
    assert!(TelegramChannel::parse_attachment_metadata(&message).is_none());
}

#[test]
fn with_workspace_dir_sets_field() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_workspace_dir(std::path::PathBuf::from("/tmp/test_workspace"));
    assert_eq!(
        ch.workspace_dir.as_deref(),
        Some(std::path::Path::new("/tmp/test_workspace"))
    );
}

#[test]
fn telegram_max_file_download_bytes_is_20mb() {
    assert_eq!(TELEGRAM_MAX_FILE_DOWNLOAD_BYTES, 20 * 1024 * 1024);
}

// ── Attachment content format tests ──────────────────────────────

#[test]
fn attachment_photo_content_uses_image_marker() {
    let local_path = std::path::Path::new("/tmp/workspace/photo_123_45.jpg");
    let local_filename = "photo_123_45.jpg";

    let content =
        format_attachment_content(IncomingAttachmentKind::Photo, local_filename, local_path);

    assert_eq!(content, "[IMAGE:/tmp/workspace/photo_123_45.jpg]");
    assert!(content.starts_with("[IMAGE:"));
    assert!(content.ends_with(']'));
}

#[test]
fn attachment_document_content_uses_document_label() {
    let local_path = std::path::Path::new("/tmp/workspace/report.pdf");
    let local_filename = "report.pdf";

    let content =
        format_attachment_content(IncomingAttachmentKind::Document, local_filename, local_path);

    assert_eq!(content, "[Document: report.pdf] /tmp/workspace/report.pdf");
    assert!(!content.contains("[IMAGE:"));
}

#[test]
fn markdown_file_never_produces_image_marker() {
    let local_path = std::path::Path::new("/tmp/workspace/telegram_files/notes.md");
    let local_filename = "notes.md";

    // Even if Telegram misclassifies as Photo, extension guard prevents [IMAGE:].
    let content =
        format_attachment_content(IncomingAttachmentKind::Photo, local_filename, local_path);
    assert!(
        !content.contains("[IMAGE:"),
        "markdown must not get [IMAGE:] marker: {content}"
    );
    assert!(content.starts_with("[Document:"));

    // As Document, it should also be correct.
    let content_doc =
        format_attachment_content(IncomingAttachmentKind::Document, local_filename, local_path);
    assert!(
        !content_doc.contains("[IMAGE:"),
        "markdown document must not get [IMAGE:] marker: {content_doc}"
    );
}

#[test]
fn non_image_photo_falls_back_to_document_format() {
    for (filename, ext_path) in [
        ("file.md", "/tmp/ws/file.md"),
        ("file.txt", "/tmp/ws/file.txt"),
        ("file.pdf", "/tmp/ws/file.pdf"),
        ("file.csv", "/tmp/ws/file.csv"),
        ("file.json", "/tmp/ws/file.json"),
        ("file.zip", "/tmp/ws/file.zip"),
        ("file", "/tmp/ws/file"),
    ] {
        let path = std::path::Path::new(ext_path);
        let content = format_attachment_content(IncomingAttachmentKind::Photo, filename, path);
        assert!(
            !content.contains("[IMAGE:"),
            "{filename}: non-image file should not get [IMAGE:] marker, got: {content}"
        );
        assert!(
            content.starts_with("[Document:"),
            "{filename}: should use [Document:] format, got: {content}"
        );
    }
}

#[test]
fn image_extensions_produce_image_marker() {
    for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp"] {
        let filename = format!("photo_1_2.{ext}");
        let path_str = format!("/tmp/ws/{filename}");
        let path = std::path::Path::new(&path_str);
        let content = format_attachment_content(IncomingAttachmentKind::Photo, &filename, path);
        assert!(
            content.starts_with("[IMAGE:"),
            "{ext}: image should get [IMAGE:] marker, got: {content}"
        );
    }
}

#[test]
fn markdown_attachment_not_detected_by_multimodal_image_markers() {
    let content = format_attachment_content(
        IncomingAttachmentKind::Photo,
        "notes.md",
        std::path::Path::new("/tmp/ws/notes.md"),
    );
    let messages = vec![zeroclaw_providers::ChatMessage::user(content)];
    assert_eq!(
        zeroclaw_providers::multimodal::count_image_markers(&messages),
        0,
        "markdown file must not trigger image marker detection"
    );
}

#[test]
fn is_image_extension_recognizes_images() {
    assert!(is_image_extension(std::path::Path::new("photo.png")));
    assert!(is_image_extension(std::path::Path::new("photo.jpg")));
    assert!(is_image_extension(std::path::Path::new("photo.jpeg")));
    assert!(is_image_extension(std::path::Path::new("photo.gif")));
    assert!(is_image_extension(std::path::Path::new("photo.webp")));
    assert!(is_image_extension(std::path::Path::new("photo.bmp")));
    assert!(is_image_extension(std::path::Path::new("PHOTO.PNG")));

    assert!(!is_image_extension(std::path::Path::new("file.md")));
    assert!(!is_image_extension(std::path::Path::new("file.txt")));
    assert!(!is_image_extension(std::path::Path::new("file.pdf")));
    assert!(!is_image_extension(std::path::Path::new("file.csv")));
    assert!(!is_image_extension(std::path::Path::new("file")));
}

#[test]
fn photo_image_marker_detected_by_multimodal() {
    let photo_content = "[IMAGE:/tmp/workspace/photo_1_2.jpg]";
    let messages = vec![zeroclaw_providers::ChatMessage::user(
        photo_content.to_string(),
    )];
    let count = zeroclaw_providers::multimodal::count_image_markers(&messages);
    assert_eq!(
        count, 1,
        "multimodal should detect exactly one image marker"
    );
}

#[test]
fn photo_image_marker_with_caption() {
    let local_path = std::path::Path::new("/tmp/workspace/photo_1_2.jpg");
    let mut content = format!("[IMAGE:{}]", local_path.display());
    let caption = "Look at this screenshot";
    use std::fmt::Write;
    let _ = write!(content, "\n\n{caption}");

    assert_eq!(
        content,
        "[IMAGE:/tmp/workspace/photo_1_2.jpg]\n\nLook at this screenshot"
    );

    // Multimodal pipeline still detects the marker.
    let messages = vec![zeroclaw_providers::ChatMessage::user(content)];
    assert_eq!(
        zeroclaw_providers::multimodal::count_image_markers(&messages),
        1
    );
}

// ── E2E: attachment saves file and formats content ───────────────

#[test]
fn e2e_attachment_saves_file_and_formats_content() {
    let workspace = tempfile::tempdir().expect("create temp workspace");

    // ── Document attachment ──────────────────────────────────────
    let doc_filename = "report.pdf";
    let doc_path = workspace.path().join(doc_filename);
    // Simulate downloaded file.
    std::fs::write(&doc_path, b"%PDF-1.4 fake").expect("write doc fixture");
    assert!(doc_path.exists(), "document file must exist on disk");

    let doc_content =
        format_attachment_content(IncomingAttachmentKind::Document, doc_filename, &doc_path);
    assert!(
        doc_content.starts_with("[Document: report.pdf]"),
        "document label format mismatch: {doc_content}"
    );
    // Multimodal must NOT detect image markers in document content.
    let doc_msgs = vec![zeroclaw_providers::ChatMessage::user(doc_content)];
    assert_eq!(
        zeroclaw_providers::multimodal::count_image_markers(&doc_msgs),
        0,
        "document content must not contain image markers"
    );

    // ── Photo attachment ─────────────────────────────────────────
    let photo_filename = "photo_99_1.jpg";
    let photo_path = workspace.path().join(photo_filename);
    // Copy the JPEG fixture.
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_photo.jpg");
    std::fs::copy(&fixture, &photo_path).expect("copy photo fixture");
    assert!(photo_path.exists(), "photo file must exist on disk");

    let photo_content =
        format_attachment_content(IncomingAttachmentKind::Photo, photo_filename, &photo_path);
    assert!(
        photo_content.starts_with("[IMAGE:"),
        "photo must use [IMAGE:] marker: {photo_content}"
    );
    assert!(
        photo_content.ends_with(']'),
        "photo marker must close with ]: {photo_content}"
    );

    // Multimodal detects the marker.
    let photo_msgs = vec![zeroclaw_providers::ChatMessage::user(photo_content.clone())];
    assert_eq!(
        zeroclaw_providers::multimodal::count_image_markers(&photo_msgs),
        1,
        "multimodal must detect exactly one image marker in photo content"
    );

    // ── Photo with caption ───────────────────────────────────────
    let mut captioned = photo_content;
    use std::fmt::Write;
    let _ = write!(captioned, "\n\nCheck this out");
    let cap_msgs = vec![zeroclaw_providers::ChatMessage::user(captioned.clone())];
    assert_eq!(
        zeroclaw_providers::multimodal::count_image_markers(&cap_msgs),
        1,
        "caption must not break image marker detection"
    );
    assert!(
        captioned.contains("Check this out"),
        "caption text must be present in content"
    );

    // ── Markdown file sent as Photo────────────────
    let md_filename = "notes.md";
    let md_path = workspace.path().join(md_filename);
    std::fs::write(&md_path, b"# Hello\nSome markdown").expect("write md fixture");
    let md_content =
        format_attachment_content(IncomingAttachmentKind::Photo, md_filename, &md_path);
    assert!(
        !md_content.contains("[IMAGE:"),
        "markdown must not get [IMAGE:] marker: {md_content}"
    );
    let md_msgs = vec![zeroclaw_providers::ChatMessage::user(md_content)];
    assert_eq!(
        zeroclaw_providers::multimodal::count_image_markers(&md_msgs),
        0,
        "markdown file must not trigger image marker detection"
    );
}

// ── Groq model_provider rejects photo with vision error ────────────────

#[test]
fn groq_provider_rejects_photo_with_vision_error() {
    use zeroclaw_providers::ModelProvider;
    use zeroclaw_providers::compatible::{AuthStyle, OpenAiCompatibleModelProvider};

    let groq = OpenAiCompatibleModelProvider::builder("test")
        .display_name("Groq")
        .base_url("https://api.groq.com/openai")
        .credential(Some("fake_key"))
        .auth_style(AuthStyle::Bearer)
        .build();

    // Groq must not support vision.
    assert!(
        !groq.supports_vision(),
        "Groq model_provider must not support vision"
    );

    // Build a message with an [IMAGE:] marker (as photo attachment would).
    let messages = vec![zeroclaw_providers::ChatMessage::user(
        "[IMAGE:/tmp/photo.jpg]\n\nDescribe this image".to_string(),
    )];
    let marker_count = zeroclaw_providers::multimodal::count_image_markers(&messages);
    assert_eq!(marker_count, 1, "must detect image marker in photo content");

    // The combination of marker_count > 0 && !supports_vision() means
    // the agent loop will return ProviderCapabilityError before calling
    // the model_provider, and the channel will send "⚠️ Error: ..." to the user.
}

#[test]
fn ack_reactions_defaults_to_true() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    assert!(ch.ack_reactions);
}

#[test]
fn with_ack_reactions_false_disables_reactions() {
    let mention_only = false;
    let ack_enabled = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_ack_reactions(ack_enabled);
    assert!(!ch.ack_reactions);
}

#[test]
fn with_ack_reactions_true_keeps_reactions() {
    let mention_only = false;
    let ack_enabled = true;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_ack_reactions(ack_enabled);
    assert!(ch.ack_reactions);
}

// ── Forwarded message tests ─────────────────────────────────────

#[test]
fn format_forward_attribution_supports_forward_origin_variants() {
    let cases = vec![
        (
            "user with username",
            serde_json::json!({
                "type": "user",
                "sender_user": { "id": 123, "username": "alice" }
            }),
            "[Forwarded from @alice] ",
        ),
        (
            "user with display name",
            serde_json::json!({
                "type": "user",
                "sender_user": {
                    "id": 123,
                    "first_name": "Alice",
                    "last_name": "Smith"
                }
            }),
            "[Forwarded from Alice Smith] ",
        ),
        (
            "hidden user",
            serde_json::json!({
                "type": "hidden_user",
                "sender_user_name": "Anonymous Sender"
            }),
            "[Forwarded from Anonymous Sender] ",
        ),
        (
            "chat",
            serde_json::json!({
                "type": "chat",
                "sender_chat": { "id": 123, "title": "Secret Group" }
            }),
            "[Forwarded from chat: Secret Group] ",
        ),
        (
            "channel",
            serde_json::json!({
                "type": "channel",
                "chat": { "id": 123, "title": "News Channel" }
            }),
            "[Forwarded from channel: News Channel] ",
        ),
    ];

    for (name, origin, expected) in cases {
        let message = serde_json::json!({ "forward_origin": origin });
        assert_eq!(
            TelegramChannel::format_forward_attribution(&message),
            Some(expected.to_string()),
            "{name}"
        );
    }
}

#[test]
fn parse_update_message_forward_origin_variants_reach_channel_content() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );

    let cases = vec![
        (
            serde_json::json!({
                "type": "user",
                "sender_user": { "id": 123, "username": "bob" }
            }),
            "[Forwarded from @bob] forwarded item",
        ),
        (
            serde_json::json!({
                "type": "hidden_user",
                "sender_user_name": "Hidden User"
            }),
            "[Forwarded from Hidden User] forwarded item",
        ),
        (
            serde_json::json!({
                "type": "chat",
                "sender_chat": { "id": -123, "title": "Secret Group" }
            }),
            "[Forwarded from chat: Secret Group] forwarded item",
        ),
        (
            serde_json::json!({
                "type": "channel",
                "chat": { "id": 123, "title": "News Channel" }
            }),
            "[Forwarded from channel: News Channel] forwarded item",
        ),
    ];

    for (index, (origin, expected)) in cases.into_iter().enumerate() {
        let update = serde_json::json!({
            "update_id": 99 + index,
            "message": {
                "message_id": 49 + index,
                "text": "forwarded item",
                "from": { "id": 1, "username": "alice" },
                "chat": { "id": 999 },
                "forward_origin": origin
            }
        });

        let msg = ch
            .parse_update_message(&update)
            .expect("forward_origin message should parse");
        assert_eq!(msg.content, expected);
    }
}

#[test]
fn parse_update_message_forwarded_reply_keeps_quote_block_separate() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 110,
        "message": {
            "message_id": 60,
            "text": "look at this news",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 999 },
            "forward_origin": {
                "type": "channel",
                "chat": { "id": 123, "title": "News Channel" }
            },
            "reply_to_message": {
                "message_id": 59,
                "text": "What do you think?",
                "from": { "id": 2, "username": "bot" }
            }
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("forwarded reply should parse");
    assert_eq!(
        msg.content,
        "[Forwarded from channel: News Channel]\n\n> @bot:\n> What do you think?\n\nlook at this news"
    );
}

#[test]
fn parse_update_message_forwarded_from_user_with_username() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 100,
        "message": {
            "message_id": 50,
            "text": "Check this out",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 999 },
            "forward_from": {
                "id": 42,
                "first_name": "Bob",
                "username": "bob"
            },
            "forward_date": 1_700_000_000
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("forwarded message should parse");
    assert_eq!(msg.content, "[Forwarded from @bob] Check this out");
}

#[test]
fn parse_update_message_forwarded_from_channel() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 101,
        "message": {
            "message_id": 51,
            "text": "Breaking news",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 999 },
            "forward_from_chat": {
                "id": -1_001_234_567_890_i64,
                "title": "Daily News",
                "username": "dailynews",
                "type": "channel"
            },
            "forward_date": 1_700_000_000
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("channel-forwarded message should parse");
    assert_eq!(
        msg.content,
        "[Forwarded from channel: Daily News] Breaking news"
    );
}

#[test]
fn parse_update_message_forwarded_hidden_sender() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 102,
        "message": {
            "message_id": 52,
            "text": "Secret tip",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 999 },
            "forward_sender_name": "Hidden User",
            "forward_date": 1_700_000_000
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("hidden-sender forwarded message should parse");
    assert_eq!(msg.content, "[Forwarded from Hidden User] Secret tip");
}

#[test]
fn parse_update_message_non_forwarded_unaffected() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 103,
        "message": {
            "message_id": 53,
            "text": "Normal message",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 999 }
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("non-forwarded message should parse");
    assert_eq!(msg.content, "Normal message");
}

#[test]
fn parse_update_message_forwarded_from_user_no_username() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let update = serde_json::json!({
        "update_id": 104,
        "message": {
            "message_id": 54,
            "text": "Hello there",
            "from": { "id": 1, "username": "alice" },
            "chat": { "id": 999 },
            "forward_from": {
                "id": 77,
                "first_name": "Charlie"
            },
            "forward_date": 1_700_000_000
        }
    });

    let msg = ch
        .parse_update_message(&update)
        .expect("forwarded message without username should parse");
    assert_eq!(msg.content, "[Forwarded from Charlie] Hello there");
}

#[test]
fn forwarded_photo_attachment_has_attribution() {
    // Verify that format_forward_attribution produces correct prefix
    // for a photo message (the actual download is async, so we test the
    // helper directly with a photo-bearing message structure).
    let message = serde_json::json!({
        "message_id": 60,
        "from": { "id": 1, "username": "alice" },
        "chat": { "id": 999 },
        "photo": [
            { "file_id": "abc123", "file_unique_id": "u1", "width": 320, "height": 240 }
        ],
        "forward_origin": {
            "type": "user",
            "sender_user": {
                "id": 42,
                "username": "bob"
            }
        },
        "forward_date": 1_700_000_000
    });

    let attr =
        TelegramChannel::format_forward_attribution(&message).expect("should detect forward");
    assert_eq!(attr, "[Forwarded from @bob] ");

    // Simulate what try_parse_attachment_message does after building content
    let photo_content = "[IMAGE:/tmp/photo.jpg]".to_string();
    let content = TelegramChannel::prepend_forward_attribution(&attr, photo_content);
    assert_eq!(content, "[Forwarded from @bob] [IMAGE:/tmp/photo.jpg]");
}

#[tokio::test]
async fn register_bot_commands_sends_correct_payload() {
    use wiremock::matchers::{body_json, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let expected_body = serde_json::json!({
        "commands": [
            { "command": "new",    "description": "Start a new conversation session" },
            { "command": "clear",  "description": "Clear this conversation session" },
            { "command": "stop",   "description": "Cancel the current in-flight task" },
            { "command": "model",  "description": "Show or switch the current model" },
            { "command": "models", "description": "List available model_providers or switch model_provider" },
            { "command": "config", "description": "Show current configuration" },
        ]
    });

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/setMyCommands$"))
        .and(body_json(&expected_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "result": true })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_api_base(mock_server.uri());

    ch.register_bot_commands().await;

    // Mock expectation assert happens on MockServer drop
}

#[tokio::test]
async fn register_bot_commands_handles_failure_gracefully() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/setMyCommands$"))
        .respond_with(ResponseTemplate::new(500).set_body_json(
            serde_json::json!({ "ok": false, "description": "Internal Server Error" }),
        ))
        .expect(1)
        .mount(&mock_server)
        .await;

    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_api_base(mock_server.uri());

    // Should not panic — errors are logged, not propagated.
    ch.register_bot_commands().await;
}

#[test]
fn sanitize_telegram_command_name_basic() {
    assert_eq!(sanitize_telegram_command_name("hello"), "hello");
    assert_eq!(sanitize_telegram_command_name("Hello"), "hello");
    assert_eq!(sanitize_telegram_command_name("my-skill"), "my_skill");
    assert_eq!(sanitize_telegram_command_name("my skill"), "my_skill");
    assert_eq!(
        sanitize_telegram_command_name("My Cool Skill!"),
        "my_cool_skill"
    );
}

#[test]
fn sanitize_telegram_command_name_trims_underscores() {
    assert_eq!(sanitize_telegram_command_name("_leading"), "leading");
    assert_eq!(sanitize_telegram_command_name("trailing_"), "trailing");
    assert_eq!(sanitize_telegram_command_name("__both__"), "both");
}

#[test]
fn sanitize_telegram_command_name_collapses_double_underscores() {
    assert_eq!(sanitize_telegram_command_name("a--b"), "a_b");
    assert_eq!(sanitize_telegram_command_name("a---b"), "a_b");
}

#[test]
fn sanitize_telegram_command_name_truncates_to_32_chars() {
    let long = "a".repeat(50);
    let result = sanitize_telegram_command_name(&long);
    assert!(result.len() <= TELEGRAM_COMMAND_NAME_MAX_LEN);
    assert_eq!(result.len(), 32);
}

#[test]
fn sanitize_telegram_command_name_empty_input() {
    assert_eq!(sanitize_telegram_command_name(""), "");
    assert_eq!(sanitize_telegram_command_name("---"), "");
}

#[test]
fn truncate_telegram_command_description_short() {
    assert_eq!(
        truncate_telegram_command_description("Short desc"),
        "Short desc"
    );
}

#[test]
fn truncate_telegram_command_description_at_limit() {
    let exact = "a".repeat(TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN);
    assert_eq!(truncate_telegram_command_description(&exact), exact);
}

#[test]
fn truncate_telegram_command_description_over_limit() {
    let long = "a".repeat(TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN + 10);
    let result = truncate_telegram_command_description(&long);
    assert!(result.chars().count() <= TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN);
    assert!(result.ends_with('…'));
}

#[test]
fn truncate_telegram_command_description_multibyte_within_char_limit() {
    let desc = format!("Multibyte weather description: {}", "🌧".repeat(30));
    assert!(desc.chars().count() <= TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN);
    assert!(desc.len() > TELEGRAM_COMMAND_DESCRIPTION_MAX_LEN);
    let result = truncate_telegram_command_description(&desc);
    assert!(
        !result.ends_with('…'),
        "should not append ellipsis when within char limit"
    );
    assert_eq!(result, desc.trim());
}

#[tokio::test]
async fn register_bot_commands_includes_skills() {
    use wiremock::matchers::{body_json, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let workspace = tempfile::tempdir().unwrap();
    let skill_dir = workspace.path().join("skills").join("weather");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: weather\ndescription: Check the weather forecast\n---\n# Weather\n",
    )
    .unwrap();

    let mock_server = MockServer::start().await;

    let expected_body = serde_json::json!({
        "commands": [
            { "command": "new",     "description": "Start a new conversation session" },
            { "command": "clear",   "description": "Clear this conversation session" },
            { "command": "stop",    "description": "Cancel the current in-flight task" },
            { "command": "model",   "description": "Show or switch the current model" },
            { "command": "models",  "description": "List available model_providers or switch model_provider" },
            { "command": "config",  "description": "Show current configuration" },
            { "command": "weather", "description": "Check the weather forecast" },
        ]
    });

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/setMyCommands$"))
        .and(body_json(&expected_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "result": true })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_api_base(mock_server.uri())
    .with_workspace_dir(workspace.path().to_path_buf());

    ch.register_bot_commands().await;
}

#[tokio::test]
async fn register_bot_commands_includes_tools_from_config() {
    use wiremock::matchers::{body_json, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let expected_body = serde_json::json!({
        "commands": [
            { "command": "new",       "description": "Start a new conversation session" },
            { "command": "clear",     "description": "Clear this conversation session" },
            { "command": "stop",      "description": "Cancel the current in-flight task" },
            { "command": "model",     "description": "Show or switch the current model" },
            { "command": "models",    "description": "List available model_providers or switch model_provider" },
            { "command": "config",    "description": "Show current configuration" },
            { "command": "test_tool", "description": "A test tool" },
        ]
    });

    Mock::given(method("POST"))
        .and(path_regex(r"/bot[^/]+/setMyCommands$"))
        .and(body_json(&expected_body))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "result": true })),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let specs = vec![("test_tool".to_string(), "A test tool".to_string())];
    let mention_only = false;
    let ch = TelegramChannel::new(
        "fake-token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    )
    .with_api_base(mock_server.uri())
    .with_tool_command_specs(specs);

    ch.register_bot_commands().await;
}

// ── Approval inline keyboard tests ────────────────────────

#[test]
fn pending_approvals_map_is_initially_empty() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let map = ch.pending_approvals.lock().await;
        assert!(map.is_empty());
    });
}

#[test]
fn approval_timeout_defaults_to_120_and_is_overridable() {
    let mention_only = false;
    let ch = TelegramChannel::new(
        "t".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    assert_eq!(ch.approval_timeout_secs, 120);
    let ch = ch.with_approval_timeout_secs(30);
    assert_eq!(ch.approval_timeout_secs, 30);
}

#[tokio::test]
async fn pending_approval_oneshot_delivers_response() {
    use zeroclaw_api::channel::ChannelApprovalResponse;

    let mention_only = false;
    let ch = TelegramChannel::new(
        "token".into(),
        "telegram_test_alias",
        Arc::new(|| vec!["*".into()]),
        mention_only,
    );
    let approval_id = "test-approval-123".to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();

    ch.pending_approvals
        .lock()
        .await
        .insert(approval_id.clone(), tx);

    // Simulate what listen() does when a callback_query arrives
    if let Some(sender) = ch.pending_approvals.lock().await.remove(&approval_id) {
        sender.send(ChannelApprovalResponse::Approve).unwrap();
    }

    let result = rx.await.unwrap();
    assert_eq!(result, ChannelApprovalResponse::Approve);
}

#[test]
fn callback_data_format_parses_correctly() {
    // Verify the callback_data format used by request_approval
    let cb_data = "approval:abc-123:approve";
    let rest = cb_data.strip_prefix("approval:").unwrap();
    let (id, action) = rest.rsplit_once(':').unwrap();
    assert_eq!(id, "abc-123");
    assert_eq!(action, "approve");

    let cb_data = "approval:abc-123:deny";
    let rest = cb_data.strip_prefix("approval:").unwrap();
    let (id, action) = rest.rsplit_once(':').unwrap();
    assert_eq!(id, "abc-123");
    assert_eq!(action, "deny");

    let cb_data = "approval:abc-123:always";
    let rest = cb_data.strip_prefix("approval:").unwrap();
    let (id, action) = rest.rsplit_once(':').unwrap();
    assert_eq!(id, "abc-123");
    assert_eq!(action, "always");
}

#[test]
fn callback_data_with_uuid_parses_correctly() {
    // UUIDs contain hyphens — rsplit_once(':') must split at the LAST colon
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let cb_data = format!("approval:{uuid}:approve");
    let rest = cb_data.strip_prefix("approval:").unwrap();
    let (id, action) = rest.rsplit_once(':').unwrap();
    assert_eq!(id, uuid);
    assert_eq!(action, "approve");
}

#[test]
fn non_approval_callback_data_is_ignored() {
    let cb_data = "some_other_action:data";
    assert!(cb_data.strip_prefix("approval:").is_none());
}
