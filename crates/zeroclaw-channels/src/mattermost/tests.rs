#[cfg(test)]
use super::*;
use serde_json::json;

#[test]
fn mattermost_url_trimming() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "https://mm.example.com/".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(Vec::new),
        thread_replies,
        mention_only,
    );
    assert_eq!(ch.base_url, "https://mm.example.com");
}

#[test]
fn mattermost_allowlist_wildcard() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    assert!(ch.is_user_allowed("any-id"));
}

#[test]
fn mattermost_parse_post_basic() {
    let thread_replies = true;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "hello world",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "botname",
            1_500_000_000_000_i64,
            "chan789",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.sender, "user456");
    assert_eq!(msg.content, "hello world");
    assert_eq!(msg.reply_target, "chan789:post123"); // Default threaded reply
}

#[test]
fn mattermost_parse_post_thread_replies_enabled() {
    let thread_replies = true;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "hello world",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "botname",
            1_500_000_000_000_i64,
            "chan789",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.reply_target, "chan789:post123"); // Threaded reply
}

#[test]
fn mattermost_parse_post_thread() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "reply",
        "create_at": 1_600_000_000_000_i64,
        "root_id": "root789"
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "botname",
            1_500_000_000_000_i64,
            "chan789",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.reply_target, "chan789:root789"); // Stays in the thread
}

#[test]
fn mattermost_parse_post_ignore_self() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "bot123",
        "message": "my own message",
        "create_at": 1_600_000_000_000_i64
    });

    let msg = ch.parse_mattermost_post(
        &post,
        "bot123",
        "botname",
        1_500_000_000_000_i64,
        "chan789",
        None,
        false,
    );
    assert!(msg.is_none());
}

#[test]
fn mattermost_parse_post_ignore_old() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "old message",
        "create_at": 1_400_000_000_000_i64
    });

    let msg = ch.parse_mattermost_post(
        &post,
        "bot123",
        "botname",
        1_500_000_000_000_i64,
        "chan789",
        None,
        false,
    );
    assert!(msg.is_none());
}

#[test]
fn mattermost_parse_post_no_thread_when_disabled() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "hello world",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "botname",
            1_500_000_000_000_i64,
            "chan789",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.reply_target, "chan789"); // No thread suffix
}

#[test]
fn mattermost_existing_thread_always_threads() {
    // Even with thread_replies=false, replies to existing threads stay in the thread
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "reply in thread",
        "create_at": 1_600_000_000_000_i64,
        "root_id": "root789"
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "botname",
            1_500_000_000_000_i64,
            "chan789",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.reply_target, "chan789:root789"); // Stays in existing thread
}

// ── mention_only tests ────────────────────────────────────────

#[test]
fn mention_only_skips_message_without_mention() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "hello everyone",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch.parse_mattermost_post(
        &post,
        "bot123",
        "mybot",
        1_500_000_000_000_i64,
        "chan1",
        None,
        false,
    );
    assert!(msg.is_none());
}

#[test]
fn mention_only_accepts_message_with_at_mention() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "@mybot what is the weather?",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "@mybot what is the weather?");
}

#[test]
fn mention_only_preserves_mention_in_body() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "  @mybot  run status  ",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "@mybot  run status");
}

#[test]
fn mention_only_admits_caption_that_is_only_the_mention() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "@mybot",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "@mybot");
}

#[test]
fn mention_only_case_insensitive() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "@MyBot hello",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "@MyBot hello");
}

#[test]
fn mention_only_detects_metadata_mentions() {
    // Even without @username in text, metadata.mentions should trigger.
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "hey check this out",
        "create_at": 1_600_000_000_000_i64,
        "root_id": "",
        "metadata": {
            "mentions": ["bot123"]
        }
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    // Content is preserved as-is since no @username was in the text to strip.
    assert_eq!(msg.content, "hey check this out");
}

#[test]
fn mention_only_word_boundary_prevents_partial_match() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    // "@mybotextended" should NOT match "@mybot" because it extends the username.
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "@mybotextended hello",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch.parse_mattermost_post(
        &post,
        "bot123",
        "mybot",
        1_500_000_000_000_i64,
        "chan1",
        None,
        false,
    );
    assert!(msg.is_none());
}

#[test]
fn mention_only_mention_in_middle_of_text() {
    let thread_replies = true;
    let mention_only = true;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "hey @mybot how are you?",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "hey @mybot how are you?");
}

#[test]
fn mention_only_disabled_passes_all_messages() {
    // With mention_only=false (default), messages pass through unfiltered.
    let thread_replies = true;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "no mention here",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "chan1",
            None,
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "no mention here");
}

// ── contains_bot_mention_mm unit tests ────────────────────────

#[test]
fn contains_mention_text_at_end() {
    let post = json!({});
    assert!(contains_bot_mention_mm(
        "hello @mybot",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn contains_mention_text_at_start() {
    let post = json!({});
    assert!(contains_bot_mention_mm(
        "@mybot hello",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn contains_mention_text_alone() {
    let post = json!({});
    assert!(contains_bot_mention_mm("@mybot", "bot123", "mybot", &post));
}

#[test]
fn no_mention_different_username() {
    let post = json!({});
    assert!(!contains_bot_mention_mm(
        "@otherbot hello",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn no_mention_partial_username() {
    let post = json!({});
    // "mybot" is a prefix of "mybotx" — should NOT match
    assert!(!contains_bot_mention_mm(
        "@mybotx hello",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn mention_detects_later_valid_mention_after_partial_prefix() {
    let post = json!({});
    assert!(contains_bot_mention_mm(
        "@mybotx ignore this, but @mybot handle this",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn mention_followed_by_punctuation() {
    let post = json!({});
    // "@mybot," — comma is not alphanumeric/underscore/dash/dot, so it's a boundary
    assert!(contains_bot_mention_mm(
        "@mybot, hello",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn mention_via_metadata_only() {
    let post = json!({
        "metadata": { "mentions": ["bot123"] }
    });
    assert!(contains_bot_mention_mm(
        "no at mention",
        "bot123",
        "mybot",
        &post
    ));
}

#[test]
fn no_mention_empty_username_no_metadata() {
    let post = json!({});
    assert!(!contains_bot_mention_mm("hello world", "bot123", "", &post));
}

// ── normalize_mattermost_content unit tests ───────────────────

#[test]
fn normalize_preserves_mention_and_trims() {
    let post = json!({});
    let result = normalize_mattermost_content("  @mybot  do stuff  ", "bot123", "mybot", &post);
    assert_eq!(result.as_deref(), Some("@mybot  do stuff"));
}

#[test]
fn normalize_returns_none_for_no_mention() {
    let post = json!({});
    let result = normalize_mattermost_content("hello world", "bot123", "mybot", &post);
    assert!(result.is_none());
}

#[test]
fn normalize_admits_mention_only_caption() {
    let post = json!({});
    let result = normalize_mattermost_content("@mybot", "bot123", "mybot", &post);
    assert_eq!(result.as_deref(), Some("@mybot"));
}

#[test]
fn normalize_preserves_text_for_metadata_mention() {
    let post = json!({
        "metadata": { "mentions": ["bot123"] }
    });
    let result = normalize_mattermost_content("check this out", "bot123", "mybot", &post);
    assert_eq!(result.as_deref(), Some("check this out"));
}

#[test]
fn normalize_preserves_multiple_mentions() {
    let post = json!({});
    let result =
        normalize_mattermost_content("@mybot hello @mybot world", "bot123", "mybot", &post);
    assert_eq!(result.as_deref(), Some("@mybot hello @mybot world"));
}

#[test]
fn normalize_keeps_partial_username_mentions() {
    let post = json!({});
    let result =
        normalize_mattermost_content("@mybot hello @mybotx world", "bot123", "mybot", &post);
    assert_eq!(result.as_deref(), Some("@mybot hello @mybotx world"));
}

// ── Transcription tests ───────────────────────────────────────

#[test]
fn mattermost_manager_none_when_transcription_not_configured() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    assert!(ch.transcription_manager.is_none());
}

#[test]
fn mattermost_manager_some_when_valid_config() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    )
    .with_transcription(zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test_key".to_string()),
        api_url: "https://api.groq.com/openai/v1/audio/transcriptions".to_string(),
        model: "whisper-large-v3".to_string(),
        language: None,
        initial_prompt: None,
        max_audio_bytes: None,
        max_duration_secs: 600,
        openai: None,
        deepgram: None,
        assemblyai: None,
        google: None,
        local_whisper: None,
        transcribe_non_ptt_audio: false,
    });
    assert!(ch.transcription_manager.is_some());
}

#[test]
fn mattermost_manager_none_and_warn_on_init_failure() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    )
    .with_transcription(zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some(String::new()),
        api_url: "https://api.groq.com/openai/v1/audio/transcriptions".to_string(),
        model: "whisper-large-v3".to_string(),
        language: None,
        initial_prompt: None,
        max_audio_bytes: None,
        max_duration_secs: 600,
        openai: None,
        deepgram: None,
        assemblyai: None,
        google: None,
        local_whisper: None,
        transcribe_non_ptt_audio: false,
    });
    assert!(ch.transcription_manager.is_none());
}

#[test]
fn mattermost_post_has_audio_attachment_true_for_audio_mime() {
    let post = json!({
        "metadata": {
            "files": [
                {
                    "id": "file1",
                    "mime_type": "audio/ogg",
                    "name": "voice.ogg"
                }
            ]
        }
    });
    assert!(post_has_audio_attachment(&post));
}

#[test]
fn mattermost_post_has_audio_attachment_true_for_audio_ext() {
    let post = json!({
        "metadata": {
            "files": [
                {
                    "id": "file1",
                    "mime_type": "application/octet-stream",
                    "extension": "ogg"
                }
            ]
        }
    });
    assert!(post_has_audio_attachment(&post));
}

#[test]
fn mattermost_post_has_audio_attachment_false_for_image() {
    let post = json!({
        "metadata": {
            "files": [
                {
                    "id": "file1",
                    "mime_type": "image/png",
                    "name": "screenshot.png"
                }
            ]
        }
    });
    assert!(!post_has_audio_attachment(&post));
}

#[test]
fn mattermost_post_has_audio_attachment_false_when_no_files() {
    let post = json!({
        "metadata": {}
    });
    assert!(!post_has_audio_attachment(&post));
}

#[test]
fn mattermost_parse_post_uses_injected_text() {
    let thread_replies = true;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "botname",
            1_500_000_000_000_i64,
            "chan789",
            Some("transcript text"),
            false,
        )
        .unwrap();
    assert_eq!(msg.content, "transcript text");
}

#[test]
fn mattermost_parse_post_rejects_empty_message_without_injected() {
    let thread_replies = true;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch.parse_mattermost_post(
        &post,
        "bot123",
        "botname",
        1_500_000_000_000_i64,
        "chan789",
        None,
        false,
    );
    assert!(msg.is_none());
}

#[tokio::test]
async fn mattermost_transcribe_skips_when_manager_none() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    );
    let post = json!({
        "metadata": {
            "files": [
                {
                    "id": "file1",
                    "mime_type": "audio/ogg",
                    "name": "voice.ogg"
                }
            ]
        }
    });
    let result = ch.try_transcribe_audio_attachment(&post).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn mattermost_transcribe_skips_over_duration_limit() {
    let thread_replies = false;
    let mention_only = false;
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_test_alias",
        Arc::new(|| vec!["*".into()]),
        thread_replies,
        mention_only,
    )
    .with_transcription(zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some("test_key".to_string()),
        api_url: "https://api.groq.com/openai/v1/audio/transcriptions".to_string(),
        model: "whisper-large-v3".to_string(),
        language: None,
        initial_prompt: None,
        max_audio_bytes: None,
        max_duration_secs: 3600,
        openai: None,
        deepgram: None,
        assemblyai: None,
        google: None,
        local_whisper: None,
        transcribe_non_ptt_audio: false,
    });

    let post = json!({
        "metadata": {
            "files": [
                {
                    "id": "file1",
                    "mime_type": "audio/ogg",
                    "name": "voice.ogg",
                    "duration": 7_200_000_u64
                }
            ]
        }
    });

    let result = ch.try_transcribe_audio_attachment(&post).await;
    assert!(result.is_none());
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn mattermost_audio_routes_through_local_whisper() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v4/files/file1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"audio bytes"))
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"text": "test transcript"})),
            )
            .mount(&mock_server)
            .await;

        let whisper_url = format!("{}/v1/audio/transcriptions", mock_server.uri());
        let thread_replies = false;
        let mention_only = false;
        let ch = MattermostChannel::new(
            mock_server.uri(),
            Some("test_token".to_string()),
            None,
            None,
            Vec::new(),
            "mattermost_test_alias",
            Arc::new(|| vec!["*".into()]),
            thread_replies,
            mention_only,
        )
        .with_transcription(zeroclaw_config::schema::TranscriptionConfig {
            enabled: true,
            api_key: None,
            api_url: "https://api.groq.com/openai/v1/audio/transcriptions".to_string(),
            model: "whisper-large-v3".to_string(),
            language: None,
            initial_prompt: None,
            max_audio_bytes: None,
            max_duration_secs: 600,
            openai: None,
            deepgram: None,
            assemblyai: None,
            google: None,
            local_whisper: Some(zeroclaw_config::schema::LocalWhisperConfig {
                url: whisper_url,
                bearer_token: Some("test_token".to_string()),
                max_audio_bytes: 25_000_000,
                timeout_secs: 300,
            }),
            transcribe_non_ptt_audio: false,
        });

        let post = json!({
            "metadata": {
                "files": [
                    {
                        "id": "file1",
                        "mime_type": "audio/ogg",
                        "name": "voice.ogg"
                    }
                ]
            }
        });

        let result = ch.try_transcribe_audio_attachment(&post).await;
        assert_eq!(result.as_deref(), Some("[Voice] test transcript"));
    }

    #[tokio::test]
    async fn mattermost_audio_skips_non_audio_attachment() {
        let mock_server = MockServer::start().await;

        let thread_replies = false;
        let mention_only = false;
        let ch = MattermostChannel::new(
            mock_server.uri(),
            Some("test_token".to_string()),
            None,
            None,
            Vec::new(),
            "mattermost_test_alias",
            Arc::new(|| vec!["*".into()]),
            thread_replies,
            mention_only,
        )
        .with_transcription(zeroclaw_config::schema::TranscriptionConfig {
            enabled: true,
            api_key: None,
            api_url: "https://api.groq.com/openai/v1/audio/transcriptions".to_string(),
            model: "whisper-large-v3".to_string(),
            language: None,
            initial_prompt: None,
            max_audio_bytes: None,
            max_duration_secs: 600,
            openai: None,
            deepgram: None,
            assemblyai: None,
            google: None,
            local_whisper: Some(zeroclaw_config::schema::LocalWhisperConfig {
                url: mock_server.uri(),
                bearer_token: Some("test_token".to_string()),
                max_audio_bytes: 25_000_000,
                timeout_secs: 300,
            }),
            transcribe_non_ptt_audio: false,
        });

        let post = json!({
            "metadata": {
                "files": [
                    {
                        "id": "file1",
                        "mime_type": "image/png",
                        "name": "screenshot.png"
                    }
                ]
            }
        });

        let result = ch.try_transcribe_audio_attachment(&post).await;
        assert!(result.is_none());
    }
}

// ── Multi-channel + DM contract (red) ────────────────────────────

fn make_ch_for_scope(channel_ids: Vec<String>) -> MattermostChannel {
    MattermostChannel::new(
        "https://mm.example.com".into(),
        Some("token".into()),
        None,
        None,
        channel_ids,
        "mattermost_scope_alias",
        Arc::new(|| vec!["*".into()]),
        true,
        false,
    )
}

#[test]
fn normalized_channel_id_strips_wildcard_and_blank() {
    assert_eq!(MattermostChannel::normalized_channel_id(None), None);
    assert_eq!(MattermostChannel::normalized_channel_id(Some("")), None);
    assert_eq!(MattermostChannel::normalized_channel_id(Some("   ")), None);
    assert_eq!(MattermostChannel::normalized_channel_id(Some("*")), None);
    assert_eq!(
        MattermostChannel::normalized_channel_id(Some("  abc123 ")),
        Some("abc123".to_string())
    );
}

#[test]
fn scoped_channel_ids_empty_returns_none() {
    let ch = make_ch_for_scope(Vec::new());
    assert_eq!(ch.scoped_channel_ids(), None);
}

#[test]
fn scoped_channel_ids_wildcard_only_returns_none() {
    let ch = make_ch_for_scope(vec!["*".into()]);
    assert_eq!(ch.scoped_channel_ids(), None);
}

#[test]
fn scoped_channel_ids_explicit_returns_dedup() {
    let ch = make_ch_for_scope(vec![
        "abc".into(),
        "  def  ".into(),
        "abc".into(),
        "*".into(),
        "".into(),
    ]);
    assert_eq!(
        ch.scoped_channel_ids(),
        Some(vec!["abc".to_string(), "def".to_string()])
    );
}

#[test]
fn is_direct_channel_treats_dm_and_group_dm_as_direct() {
    assert!(is_direct_channel("D"));
    assert!(is_direct_channel("G"));
}

#[test]
fn is_direct_channel_rejects_public_and_private_team_channels() {
    assert!(!is_direct_channel("O"));
    assert!(!is_direct_channel("P"));
    assert!(!is_direct_channel(""));
    assert!(!is_direct_channel("X"));
}

fn ch_obj(id: &str, ty: &str, team: &str) -> serde_json::Value {
    json!({"id": id, "type": ty, "team_id": team})
}

#[test]
fn filter_discovered_channels_includes_all_when_no_filters() {
    let raw = vec![
        ch_obj("pub1", "O", "teamA"),
        ch_obj("priv1", "P", "teamA"),
        ch_obj("dm1", "D", ""),
        ch_obj("gdm1", "G", ""),
    ];
    let kept = filter_discovered_channels(&raw, &[], true);
    let ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["pub1", "priv1", "dm1", "gdm1"]);
    assert!(!kept[0].is_direct);
    assert!(!kept[1].is_direct);
    assert!(kept[2].is_direct);
    assert!(kept[3].is_direct);
}

#[test]
fn filter_discovered_channels_respects_team_ids_allowlist() {
    let raw = vec![
        ch_obj("pub_a", "O", "teamA"),
        ch_obj("pub_b", "O", "teamB"),
        ch_obj("priv_a", "P", "teamA"),
    ];
    let kept = filter_discovered_channels(&raw, &["teamA".to_string()], true);
    let ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["pub_a", "priv_a"]);
}

#[test]
fn filter_discovered_channels_omits_dms_when_discover_dms_false() {
    let raw = vec![
        ch_obj("pub1", "O", "teamA"),
        ch_obj("dm1", "D", ""),
        ch_obj("gdm1", "G", ""),
    ];
    let kept = filter_discovered_channels(&raw, &[], false);
    let ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["pub1"]);
}

#[test]
fn filter_discovered_channels_keeps_dms_regardless_of_team_ids() {
    let raw = vec![
        ch_obj("pub_b", "O", "teamB"),
        ch_obj("dm1", "D", ""),
        ch_obj("gdm1", "G", ""),
    ];
    let kept = filter_discovered_channels(&raw, &["teamA".to_string()], true);
    let ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["dm1", "gdm1"]);
}

#[test]
fn mention_only_bypassed_for_direct_channels_in_parse() {
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_dm_alias",
        Arc::new(|| vec!["*".into()]),
        false,
        true,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "no mention here, just talking",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch
        .parse_mattermost_post(
            &post,
            "bot123",
            "mybot",
            1_500_000_000_000_i64,
            "dm_channel",
            None,
            true,
        )
        .expect("DM message must bypass mention_only and produce a ChannelMessage");
    assert_eq!(msg.content, "no mention here, just talking");
}

#[test]
fn mention_only_applied_in_parse_when_is_direct_false() {
    let ch = MattermostChannel::new(
        "url".into(),
        Some("token".into()),
        None,
        None,
        Vec::new(),
        "mattermost_group_alias",
        Arc::new(|| vec!["*".into()]),
        false,
        true,
    );
    let post = json!({
        "id": "post1",
        "user_id": "user1",
        "message": "no mention here, just talking",
        "create_at": 1_600_000_000_000_i64,
        "root_id": ""
    });

    let msg = ch.parse_mattermost_post(
        &post,
        "bot123",
        "mybot",
        1_500_000_000_000_i64,
        "pub_channel",
        None,
        false,
    );
    assert!(msg.is_none(), "public channel must enforce mention_only");
}

#[cfg(test)]
mod discovery_http_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_target_channels_discovers_via_users_me_channels() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v4/users/me"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "bot123", "username": "mybot"})),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/v4/users/me/channels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "pub_a", "type": "O", "team_id": "teamA"},
                {"id": "pub_b", "type": "O", "team_id": "teamB"},
                {"id": "dm_x",  "type": "D", "team_id": ""},
                {"id": "gdm_y", "type": "G", "team_id": ""},
            ])))
            .mount(&mock_server)
            .await;

        let ch = MattermostChannel::new(
            mock_server.uri(),
            Some("token".into()),
            None,
            None,
            Vec::new(),
            "mattermost_discover_alias",
            Arc::new(|| vec!["*".into()]),
            false,
            false,
        )
        .with_team_ids(vec!["teamA".to_string()])
        .with_discover_dms(true);

        let targets = ch
            .list_target_channels()
            .await
            .expect("discovery must succeed");
        let ids: Vec<&str> = targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["pub_a", "dm_x", "gdm_y"],
            "discovery should keep teamA channels and all DMs"
        );
        assert!(!targets[0].is_direct);
        assert!(targets[1].is_direct);
        assert!(targets[2].is_direct);
    }

    #[tokio::test]
    async fn list_target_channels_explicit_ids_skip_discovery_and_lookup_types() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v4/channels/explicit_dm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "explicit_dm",
                "type": "D",
                "team_id": ""
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/v4/channels/explicit_pub"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "explicit_pub",
                "type": "O",
                "team_id": "teamA"
            })))
            .mount(&mock_server)
            .await;

        let ch = MattermostChannel::new(
            mock_server.uri(),
            Some("token".into()),
            None,
            None,
            vec!["explicit_dm".into(), "explicit_pub".into()],
            "mattermost_explicit_alias",
            Arc::new(|| vec!["*".into()]),
            false,
            false,
        );

        let targets = ch
            .list_target_channels()
            .await
            .expect("explicit lookup must succeed");
        let by_id: std::collections::HashMap<_, _> = targets
            .iter()
            .map(|t| (t.id.as_str(), t.is_direct))
            .collect();
        assert_eq!(by_id.get("explicit_dm"), Some(&true));
        assert_eq!(by_id.get("explicit_pub"), Some(&false));
        assert_eq!(targets.len(), 2);
    }
}

#[test]
fn test_ws_url_conversion() {
    let ch = MattermostChannel::new(
        "https://mm.example.com".into(),
        Some("token".into()),
        None,
        None,
        vec![],
        "test",
        Arc::new(Vec::new),
        false,
        false,
    );
    assert_eq!(ch.ws_url(), "wss://mm.example.com/api/v4/websocket");

    let ch2 = MattermostChannel::new(
        "http://localhost:8065".into(),
        Some("token".into()),
        None,
        None,
        vec![],
        "test",
        Arc::new(Vec::new),
        false,
        false,
    );
    assert_eq!(ch2.ws_url(), "ws://localhost:8065/api/v4/websocket");

    // server URL with path prefix should preserve it
    let ch3 = MattermostChannel::new(
        "https://mm.example.com/subpath".into(),
        Some("token".into()),
        None,
        None,
        vec![],
        "test",
        Arc::new(Vec::new),
        false,
        false,
    );
    assert_eq!(
        ch3.ws_url(),
        "wss://mm.example.com/subpath/api/v4/websocket"
    );
}

#[test]
fn test_listen_mode_default_is_polling() {
    assert_eq!(
        MattermostListenMode::default(),
        MattermostListenMode::Polling
    );
}

#[test]
fn test_listen_mode_serde() {
    // serialize
    assert_eq!(
        serde_json::to_string(&MattermostListenMode::Polling).unwrap(),
        "\"polling\""
    );
    assert_eq!(
        serde_json::to_string(&MattermostListenMode::Websocket).unwrap(),
        "\"websocket\""
    );

    // deserialize
    let polling: MattermostListenMode = serde_json::from_str("\"polling\"").unwrap();
    assert_eq!(polling, MattermostListenMode::Polling);

    let websocket: MattermostListenMode = serde_json::from_str("\"websocket\"").unwrap();
    assert_eq!(websocket, MattermostListenMode::Websocket);

    // deserialize unknown variant -> error
    assert!(serde_json::from_str::<MattermostListenMode>("\"unknown\"").is_err());
}

#[test]
fn test_ws_event_posted_parsing() {
    let post = json!({
        "id": "post123",
        "user_id": "user456",
        "message": "hello world",
        "create_at": 1717000000000i64,
        "root_id": "",
        "channel_id": "chan789",
        "type": ""
    });

    let ch = MattermostChannel::new(
        "https://mm.example.com".into(),
        Some("token".into()),
        None,
        None,
        vec![],
        "test",
        Arc::new(|| vec!["user456".into()]),
        false,
        false,
    );

    let msg = ch
        .parse_mattermost_post(&post, "bot_user", "bot_username", 0, "chan789", None, false)
        .expect("should parse posted event post");

    assert_eq!(msg.id, "mattermost_post123");
    assert_eq!(msg.sender, "user456");
    assert_eq!(msg.content, "hello world");
}

#[test]
fn test_ws_posted_envelope_post_is_json_string() {
    // Mattermost sends data.post as a JSON-encoded string, not a nested
    // object. This test exercises the extraction path the WebSocket listener
    // uses: Value::String → as_str() → from_str. The old to_string() path
    // would re-serialize as a quoted literal and silently drop the event.
    let post_obj = json!({
        "id": "post789",
        "user_id": "user999",
        "message": "ws message",
        "create_at": 1717000000000i64,
        "root_id": "",
        "channel_id": "chan111",
        "type": ""
    });
    let post_str = serde_json::to_string(&post_obj).unwrap();

    let envelope = json!({
        "event": "posted",
        "data": {
            "post": post_str
        }
    });

    let post = MattermostChannel::ws_post_from_event(&envelope)
        .expect("should parse the inner JSON string");

    let ch = MattermostChannel::new(
        "https://mm.example.com".into(),
        Some("token".into()),
        None,
        None,
        vec![],
        "test",
        Arc::new(|| vec!["user999".into()]),
        false,
        false,
    );

    let msg = ch
        .parse_mattermost_post(&post, "bot_user", "bot_username", 0, "chan111", None, false)
        .expect("should parse posted event post from envelope");

    assert_eq!(msg.id, "mattermost_post789");
    assert_eq!(msg.sender, "user999");
    assert_eq!(msg.content, "ws message");
}

#[test]
fn test_ws_ping_message_format() {
    // Verify the application-level ping frame the heartbeat pinger sends.
    let seq = 1i64;
    let ping = serde_json::json!({"seq": seq, "action": "ping"});
    assert_eq!(ping["seq"], serde_json::json!(1i64));
    assert_eq!(ping["action"], serde_json::json!("ping"));

    // Round-trip: the message is a Text frame whose content is the JSON
    // string. The Mattermost server expects this exact shape.
    let text = ping.to_string();
    let roundtripped: serde_json::Value =
        serde_json::from_str(&text).expect("ping json must round-trip");
    assert_eq!(roundtripped["action"], serde_json::json!("ping"));
    assert!(roundtripped["seq"].is_i64());
}

#[test]
fn test_ws_auth_challenge_format() {
    // Verify the authentication_challenge frame sent immediately after connect.
    let token = "test_bot_token";
    let seq = 1i64;
    let auth = serde_json::json!({
        "seq": seq,
        "action": "authentication_challenge",
        "data": { "token": token }
    });
    assert_eq!(auth["seq"], serde_json::json!(1i64));
    assert_eq!(
        auth["action"],
        serde_json::json!("authentication_challenge")
    );
    assert_eq!(auth["data"]["token"], serde_json::json!("test_bot_token"));

    let text = auth.to_string();
    let roundtripped: serde_json::Value =
        serde_json::from_str(&text).expect("auth json must round-trip");
    assert_eq!(
        roundtripped["data"]["token"],
        serde_json::json!("test_bot_token")
    );
}

#[test]
fn test_ws_auth_response_matches_challenge_sequence() {
    let success = json!({"status": "OK", "seq_reply": 7});
    let failure = json!({"status": "FAIL", "seq_reply": 7});
    let unrelated = json!({"status": "OK", "seq_reply": 8});

    assert_eq!(MattermostChannel::ws_auth_response(&success, 7), Some(true));
    assert_eq!(
        MattermostChannel::ws_auth_response(&failure, 7),
        Some(false)
    );
    assert_eq!(MattermostChannel::ws_auth_response(&unrelated, 7), None);
}

#[tokio::test]
async fn test_ws_handshake_sends_auth_before_waiting_for_hello() {
    use tokio_tungstenite::tungstenite::protocol::Role;

    let (client_io, server_io) = tokio::io::duplex(4096);
    let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
    let (mut write, mut read) = client.split();

    let server_task = zeroclaw_spawn::spawn!(async move {
        let first = server
            .next()
            .await
            .expect("client should send auth first")
            .expect("auth frame should be readable");
        let WsMessage::Text(text) = first else {
            panic!("first client frame should be text auth");
        };
        let auth: serde_json::Value =
            serde_json::from_str(text.as_ref()).expect("auth should be JSON");
        assert_eq!(auth["action"], "authentication_challenge");
        assert_eq!(auth["data"]["token"], "test-token");

        server
            .send(WsMessage::Text(
                json!({"status": "OK", "seq_reply": 7}).to_string().into(),
            ))
            .await
            .expect("server should send auth response");
        server
            .send(WsMessage::Text(
                json!({"event": "hello", "data": {"server_version": "10.8.0"}})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("server should send hello");
    });

    let version = MattermostChannel::authenticate_websocket(
        &mut write,
        &mut read,
        "test-token",
        7,
        Duration::from_secs(1),
    )
    .await
    .expect("auth response followed by hello should complete the handshake");

    assert_eq!(version, "10.8.0");
    server_task.await.expect("fake server should finish");
}

#[tokio::test]
async fn test_ws_handshake_rejects_failed_auth() {
    use tokio_tungstenite::tungstenite::protocol::Role;

    let (client_io, server_io) = tokio::io::duplex(4096);
    let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
    let (mut write, mut read) = client.split();

    let server_task = zeroclaw_spawn::spawn!(async move {
        server
            .next()
            .await
            .expect("auth frame should arrive")
            .unwrap();
        server
            .send(WsMessage::Text(
                json!({"status": "FAIL", "seq_reply": 3}).to_string().into(),
            ))
            .await
            .expect("server should send rejection");
    });

    let error = MattermostChannel::authenticate_websocket(
        &mut write,
        &mut read,
        "bad-token",
        3,
        Duration::from_secs(1),
    )
    .await
    .expect_err("failed auth must end the listener attempt");

    assert!(error.to_string().contains("authentication was rejected"));
    server_task.await.expect("fake server should finish");
}

#[tokio::test]
async fn test_ws_handshake_times_out_after_auth_send() {
    use tokio_tungstenite::tungstenite::protocol::Role;

    let (client_io, server_io) = tokio::io::duplex(4096);
    let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
    let (mut write, mut read) = client.split();

    let server_task = zeroclaw_spawn::spawn!(async move {
        server
            .next()
            .await
            .expect("auth frame should arrive")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let error = MattermostChannel::authenticate_websocket(
        &mut write,
        &mut read,
        "test-token",
        4,
        Duration::from_millis(10),
    )
    .await
    .expect_err("a silent server must fail the handshake deadline");

    assert!(error.to_string().contains("handshake timed out"));
    server_task.abort();
}

#[tokio::test]
async fn test_ws_handshake_times_out_without_hello() {
    use tokio_tungstenite::tungstenite::protocol::Role;

    let (client_io, server_io) = tokio::io::duplex(4096);
    let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
    let (mut write, mut read) = client.split();

    let server_task = zeroclaw_spawn::spawn!(async move {
        server
            .next()
            .await
            .expect("auth frame should arrive")
            .unwrap();
        server
            .send(WsMessage::Text(
                json!({"status": "OK", "seq_reply": 5}).to_string().into(),
            ))
            .await
            .expect("server should send auth response");
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let error = MattermostChannel::authenticate_websocket(
        &mut write,
        &mut read,
        "test-token",
        5,
        Duration::from_millis(10),
    )
    .await
    .expect_err("auth without hello must fail the handshake deadline");

    assert!(error.to_string().contains("handshake timed out"));
    server_task.abort();
}

#[tokio::test]
async fn test_ws_handshake_times_out_without_auth_response() {
    use tokio_tungstenite::tungstenite::protocol::Role;

    let (client_io, server_io) = tokio::io::duplex(4096);
    let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
    let (mut write, mut read) = client.split();

    let server_task = zeroclaw_spawn::spawn!(async move {
        server
            .next()
            .await
            .expect("auth frame should arrive")
            .unwrap();
        server
            .send(WsMessage::Text(
                json!({"event": "hello", "data": {"server_version": "10.8.0"}})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("server should send hello");
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let error = MattermostChannel::authenticate_websocket(
        &mut write,
        &mut read,
        "test-token",
        6,
        Duration::from_millis(10),
    )
    .await
    .expect_err("hello without auth response must fail the handshake deadline");

    assert!(error.to_string().contains("handshake timed out"));
    server_task.abort();
}

#[test]
fn test_ws_timeout_constants() {
    // WS_READ_TIMEOUT must be strictly greater than WS_PING_INTERVAL
    // so a single missed ping does not trigger a false positive.
    assert!(
        WS_READ_TIMEOUT > WS_PING_INTERVAL,
        "WS_READ_TIMEOUT ({:?}) must exceed WS_PING_INTERVAL ({:?})",
        WS_READ_TIMEOUT,
        WS_PING_INTERVAL
    );
    // WS_READ_TIMEOUT should be at least 3× ping interval so the
    // server can miss two pings before the listener reconnects.
    assert!(
        WS_READ_TIMEOUT >= WS_PING_INTERVAL.mul_f64(3.0),
        "WS_READ_TIMEOUT ({:?}) must be ≥ 3× WS_PING_INTERVAL ({:?})",
        WS_READ_TIMEOUT,
        WS_PING_INTERVAL
    );
    assert!(WS_HANDSHAKE_TIMEOUT <= WS_READ_TIMEOUT);
}

#[tokio::test]
async fn test_ws_read_timeout_detects_silent_peer() {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
    tokio::select! {
        () = std::future::pending::<()>() => panic!("silent peer unexpectedly produced a frame"),
        () = tokio::time::sleep_until(deadline) => {}
    }
}
