use super::*;

#[test]
fn split_text_into_chunks_safe_on_multibyte_utf8() {
    let text = format!(
        "{}{}{}",
        "a".repeat(SLACK_BLOCK_TEXT_MAX_CHARS - 1),
        "😀",
        "tail"
    );
    let chunks = split_text_into_chunks(&text, SLACK_BLOCK_TEXT_MAX_CHARS, 3);

    assert_eq!(chunks.concat(), text);
    assert_eq!(chunks[0].len(), SLACK_BLOCK_TEXT_MAX_CHARS - 1);
    assert_eq!(chunks[1], "😀tail");
    for chunk in &chunks {
        assert!(chunk.len() <= SLACK_BLOCK_TEXT_MAX_CHARS);
        assert!(chunk.is_char_boundary(chunk.len()));
    }
}

#[test]
fn slack_channel_name() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.name(), "slack");
}

#[test]
fn slack_channel_with_channel_ids() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec!["C12345".into()],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.channel_ids, vec!["C12345".to_string()]);
}

#[test]
fn slack_group_reply_policy_defaults_to_all_messages() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["*".into()]),
    );
    assert!(ch.thread_replies);
    assert!(!ch.mention_only);
    assert!(ch.group_reply_allowed_sender_ids.is_empty());
}

#[test]
fn with_thread_replies_sets_flag() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_thread_replies(false);
    assert!(!ch.thread_replies);
}

#[test]
fn with_strict_mention_in_thread_sets_flag() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert!(!ch.strict_mention_in_thread);
    let ch = ch.with_strict_mention_in_thread(true);
    assert!(ch.strict_mention_in_thread);
}

#[test]
fn outbound_thread_ts_respects_thread_replies_setting() {
    let msg = SendMessage::new("hello", "C123").in_thread(Some("1741234567.100001".into()));

    let threaded = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(threaded.outbound_thread_ts(&msg), Some("1741234567.100001"));

    let channel_root = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_thread_replies(false);
    assert_eq!(channel_root.outbound_thread_ts(&msg), None);
}

#[test]
fn with_workspace_dir_sets_field() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_workspace_dir(PathBuf::from("/tmp/slack-workspace"));
    assert_eq!(
        ch.workspace_dir.as_deref(),
        Some(std::path::Path::new("/tmp/slack-workspace"))
    );
}

#[test]
fn slack_group_reply_policy_applies_sender_overrides() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["*".into()]),
    )
    .with_group_reply_policy(true, vec![" U111 ".into(), "U111".into(), "U222".into()]);

    assert!(ch.mention_only);
    assert_eq!(
        ch.group_reply_allowed_sender_ids,
        vec!["U111".to_string(), "U222".to_string()]
    );
    assert!(ch.is_group_sender_trigger_enabled("U111"));
    assert!(!ch.is_group_sender_trigger_enabled("U999"));
}

#[test]
fn normalized_channel_id_respects_wildcard_and_blank() {
    assert_eq!(SlackChannel::normalized_channel_id(None), None);
    assert_eq!(SlackChannel::normalized_channel_id(Some("")), None);
    assert_eq!(SlackChannel::normalized_channel_id(Some("   ")), None);
    assert_eq!(SlackChannel::normalized_channel_id(Some("*")), None);
    assert_eq!(SlackChannel::normalized_channel_id(Some(" * ")), None);
    assert_eq!(
        SlackChannel::normalized_channel_id(Some(" C12345 ")),
        Some("C12345".to_string())
    );
}

#[test]
fn configured_app_token_ignores_blank_values() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        Some("   ".into()),
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.configured_app_token(), None);
}

#[test]
fn configured_app_token_trims_value() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        Some(" xapp-123 ".into()),
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.configured_app_token().as_deref(), Some("xapp-123"));
}

#[test]
fn scoped_channel_ids_uses_explicit_list() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec!["C_LIST1".into(), "D_DM1".into()],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(
        ch.scoped_channel_ids(),
        Some(vec!["C_LIST1".to_string(), "D_DM1".to_string()])
    );
}

#[test]
fn scoped_channel_ids_with_single_entry() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec!["C_SINGLE".into()],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.scoped_channel_ids(), Some(vec!["C_SINGLE".to_string()]));
}

#[test]
fn scoped_channel_ids_returns_none_for_wildcard_mode() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.scoped_channel_ids(), None);
}

#[test]
fn is_group_channel_id_detects_channel_prefixes() {
    assert!(SlackChannel::is_group_channel_id("C123"));
    assert!(SlackChannel::is_group_channel_id("G123"));
    assert!(!SlackChannel::is_group_channel_id("D123"));
    assert!(!SlackChannel::is_group_channel_id(""));
}

#[test]
fn is_direct_message_true_for_im_reply_target() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    let dm = zeroclaw_api::channel::ChannelMessage {
        reply_target: "D0B189MTELX".into(),
        channel: "slack".into(),
        ..Default::default()
    };
    let group = zeroclaw_api::channel::ChannelMessage {
        reply_target: "C12345".into(),
        ..dm.clone()
    };
    assert!(Channel::is_direct_message(&ch, &dm));
    assert!(!Channel::is_direct_message(&ch, &group));
}

#[test]
fn extract_channel_ids_filters_archived_and_non_member_entries() {
    let payload = serde_json::json!({
        "channels": [
            {"id": "C1", "is_archived": false, "is_member": true},
            {"id": "C2", "is_archived": true, "is_member": true},
            {"id": "C3", "is_archived": false, "is_member": false},
            {"id": "C1", "is_archived": false, "is_member": true},
            {"id": "C4"}
        ]
    });
    let ids = SlackChannel::extract_channel_ids(&payload);
    assert_eq!(ids, vec!["C1".to_string(), "C4".to_string()]);
}

#[test]
fn empty_allowlist_denies_everyone() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert!(!ch.is_user_allowed("U12345"));
    assert!(!ch.is_user_allowed("anyone"));
}

#[test]
fn wildcard_allows_everyone() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["*".into()]),
    );
    assert!(ch.is_user_allowed("U12345"));
}

#[test]
fn explicit_user_peer_is_allowed() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["U01EXAMPLE".into()]),
    );
    assert!(ch.is_user_allowed("U01EXAMPLE"));
    assert!(!ch.is_user_allowed("U99OTHER"));
}

#[test]
fn extract_user_display_name_prefers_profile_display_name() {
    let payload = serde_json::json!({
        "ok": true,
        "user": {
            "name": "fallback_name",
            "profile": {
                "display_name": "Display Name",
                "real_name": "Real Name"
            }
        }
    });

    assert_eq!(
        SlackChannel::extract_user_display_name(&payload).as_deref(),
        Some("Display Name")
    );
}

#[test]
fn extract_user_display_name_falls_back_to_username() {
    let payload = serde_json::json!({
        "ok": true,
        "user": {
            "name": "fallback_name",
            "profile": {
                "display_name": "   ",
                "real_name": ""
            }
        }
    });

    assert_eq!(
        SlackChannel::extract_user_display_name(&payload).as_deref(),
        Some("fallback_name")
    );
}

#[test]
fn cached_sender_display_name_returns_none_when_expired() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["*".into()]),
    );
    {
        let mut cache = ch.user_display_name_cache.lock().unwrap();
        cache.insert(
            "U123".to_string(),
            CachedSlackDisplayName {
                display_name: "Expired Name".to_string(),
                expires_at: Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("instant should allow subtracting one second in tests"),
            },
        );
    }

    assert_eq!(ch.cached_sender_display_name("U123"), None);
}

#[test]
fn cached_sender_display_name_returns_cached_value_when_valid() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["*".into()]),
    );
    ch.cache_sender_display_name("U123", "Cached Name");

    assert_eq!(
        ch.cached_sender_display_name("U123").as_deref(),
        Some("Cached Name")
    );
}

#[test]
fn normalize_incoming_content_requires_mention_when_enabled() {
    assert!(SlackChannel::normalize_incoming_content("hello", true, "U_BOT").is_none());
    assert_eq!(
        SlackChannel::normalize_incoming_content("<@U_BOT> run", true, "U_BOT").as_deref(),
        Some("<@U_BOT> run")
    );
}

#[test]
fn normalize_incoming_content_without_mention_mode_keeps_message() {
    assert_eq!(
        SlackChannel::normalize_incoming_content("  hello world  ", false, "U_BOT").as_deref(),
        Some("hello world")
    );
}

#[test]
fn compose_incoming_content_allows_attachment_only_messages() {
    let composed = SlackChannel::compose_incoming_content(
        String::new(),
        vec!["[IMAGE:data:image/png;base64,aaaa]".to_string()],
    );
    assert_eq!(
        composed.as_deref(),
        Some("[IMAGE:data:image/png;base64,aaaa]")
    );
}

#[test]
fn parse_slack_permalink_accepts_standard_archives_link() {
    let parsed = SlackChannel::parse_slack_permalink(
        "https://acme.slack.com/archives/C12345678/p1712345678901234",
    )
    .expect("permalink");

    assert_eq!(parsed.channel_id, "C12345678");
    assert_eq!(parsed.message_ts, "1712345678.901234");
    assert_eq!(parsed.thread_ts_hint, None);
}

#[test]
fn parse_slack_permalink_reads_thread_hint_when_present() {
    let parsed = SlackChannel::parse_slack_permalink(
            "https://acme.slack.com/archives/C12345678/p1712345678901234?thread_ts=1712345600.000100&cid=C12345678",
        )
        .expect("permalink");

    assert_eq!(parsed.thread_ts_hint.as_deref(), Some("1712345600.000100"));
}

#[test]
fn parse_slack_permalink_rejects_non_message_links() {
    assert!(SlackChannel::parse_slack_permalink("https://example.com/path").is_none());
    assert!(SlackChannel::parse_slack_permalink("https://acme.slack.com/client/T1/C1").is_none());
    assert!(
        SlackChannel::parse_slack_permalink("https://acme.slack.com/archives/C1/not-a-message")
            .is_none()
    );
}

#[test]
fn extract_slack_permalinks_handles_slack_angle_bracket_format() {
    let permalinks = SlackChannel::extract_slack_permalinks(
        "Please inspect <https://acme.slack.com/archives/C123/p1712345678901234|message> now",
    );

    assert_eq!(permalinks.len(), 1);
    assert_eq!(permalinks[0].channel_id, "C123");
    assert_eq!(permalinks[0].message_ts, "1712345678.901234");
}

#[test]
fn extract_slack_permalinks_deduplicates_message_targets() {
    let permalinks = SlackChannel::extract_slack_permalinks(
        "https://acme.slack.com/archives/C123/p1712345678901234 again <https://acme.slack.com/archives/C123/p1712345678901234|same>",
    );

    assert_eq!(permalinks.len(), 1);
}

#[test]
fn message_subtype_support_allows_file_share() {
    assert!(SlackChannel::is_supported_message_subtype(None));
    assert!(SlackChannel::is_supported_message_subtype(Some(
        "file_share"
    )));
    assert!(SlackChannel::is_supported_message_subtype(Some(
        "thread_broadcast"
    )));
    assert!(!SlackChannel::is_supported_message_subtype(Some(
        "message_changed"
    )));
    assert!(!SlackChannel::is_supported_message_subtype(Some(
        "channel_join"
    )));
}

#[test]
fn file_text_preview_prefers_preview_field() {
    let file = serde_json::json!({
        "preview": "line 1\nline 2",
        "preview_highlight": "ignored"
    });
    assert_eq!(
        SlackChannel::file_text_preview(&file).as_deref(),
        Some("line 1\nline 2")
    );
}

#[test]
fn is_image_file_detects_mimetype_or_extension() {
    let from_mime = serde_json::json!({"mimetype":"image/png"});
    let from_ext = serde_json::json!({"name":"photo.jpeg"});
    let non_image = serde_json::json!({"name":"notes.txt","mimetype":"text/plain"});
    assert!(SlackChannel::is_image_file(&from_mime));
    assert!(SlackChannel::is_image_file(&from_ext));
    assert!(!SlackChannel::is_image_file(&non_image));
}

#[test]
fn detect_image_mime_rejects_non_image_bytes_despite_image_metadata() {
    let file = serde_json::json!({"mimetype":"image/png","name":"wow.png"});
    let html_bytes = b"<!DOCTYPE html><html><body>login required</body></html>";
    assert_eq!(
        SlackChannel::detect_image_mime(
            Some("image/png"),
            &file,
            html_bytes,
            "https://files.slack.com/files-pri/T1/F2/wow.png"
        ),
        None
    );
}

#[test]
fn detect_image_mime_prefers_magic_bytes_over_misleading_metadata() {
    let file = serde_json::json!({"mimetype":"image/bmp","name":"wow.png"});
    let png_header = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    assert_eq!(
        SlackChannel::detect_image_mime(
            Some("image/bmp"),
            &file,
            &png_header,
            "https://files.slack.com/files-pri/T1/F2/wow.png"
        )
        .as_deref(),
        Some("image/png")
    );
}

#[test]
fn is_probably_text_file_accepts_snippet_mode() {
    let snippet = serde_json::json!({"mode":"snippet"});
    let plain = serde_json::json!({"mimetype":"text/plain"});
    let binary = serde_json::json!({"mimetype":"application/octet-stream","name":"a.bin"});
    assert!(SlackChannel::is_probably_text_file(&snippet));
    assert!(SlackChannel::is_probably_text_file(&plain));
    assert!(!SlackChannel::is_probably_text_file(&binary));
}

#[test]
fn sanitize_attachment_filename_strips_path_traversal() {
    assert_eq!(
        SlackChannel::sanitize_attachment_filename("../../secret.txt").as_deref(),
        Some("secret.txt")
    );
    assert_eq!(
        SlackChannel::sanitize_attachment_filename(r"..\\..\\secret.txt").as_deref(),
        Some("..__..__secret.txt")
    );
    assert!(SlackChannel::sanitize_attachment_filename("..").is_none());
}

#[test]
fn parse_outbound_attachment_markers_extracts_supported_markers() {
    let (cleaned, attachments) =
        parse_outbound_attachment_markers("Done [IMAGE:/tmp/chart.png] and [file:/tmp/a.pdf]");

    assert_eq!(cleaned, "Done  and");
    assert_eq!(attachments.len(), 2);
    assert_eq!(attachments[0].kind, SlackOutboundAttachmentKind::Image);
    assert_eq!(attachments[0].target, "/tmp/chart.png");
    assert_eq!(attachments[1].kind, SlackOutboundAttachmentKind::File);
    assert_eq!(attachments[1].target, "/tmp/a.pdf");
}

#[test]
fn parse_outbound_attachment_markers_keeps_unknown_markers() {
    let (cleaned, attachments) =
        parse_outbound_attachment_markers("Keep [UNKNOWN:/tmp/chart.png] here");

    assert_eq!(cleaned, "Keep [UNKNOWN:/tmp/chart.png] here");
    assert!(attachments.is_empty());
}

#[tokio::test]
async fn resolve_outbound_attachment_marker_accepts_workspace_file() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("chart.png");
    tokio::fs::write(&path, b"\x89PNG\r\n\x1a\n").await.unwrap();
    let channel = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_workspace_dir(workspace.path().to_path_buf());
    let marker = SlackOutboundAttachmentMarker {
        kind: SlackOutboundAttachmentKind::Image,
        target: path.to_string_lossy().to_string(),
    };

    let attachment = channel
        .resolve_outbound_attachment_marker(&marker)
        .await
        .unwrap();

    assert_eq!(attachment.file_name, "chart.png");
    assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
    assert_eq!(attachment.data, b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn resolve_outbound_attachment_marker_rejects_workspace_escape() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("secret.png");
    tokio::fs::write(&path, b"\x89PNG\r\n\x1a\n").await.unwrap();
    let channel = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_workspace_dir(workspace.path().to_path_buf());
    let marker = SlackOutboundAttachmentMarker {
        kind: SlackOutboundAttachmentKind::Image,
        target: path.to_string_lossy().to_string(),
    };

    let err = channel
        .resolve_outbound_attachment_marker(&marker)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("escapes workspace"), "{err}");
}

async fn mock_slack_upload_flow(
    server: &wiremock::MockServer,
    file_id: &str,
    upload_path: &str,
    complete_status: u16,
    complete_body: serde_json::Value,
) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("POST"))
        .and(path("/files.getUploadURLExternal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "upload_url": format!("{}{}", server.uri(), upload_path),
            "file_id": file_id,
        })))
        .expect(1)
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path(upload_path))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/files.completeUploadExternal"))
        .respond_with(ResponseTemplate::new(complete_status).set_body_json(complete_body))
        .expect(1)
        .mount(server)
        .await;
}

fn test_slack_channel(server: &wiremock::MockServer, workspace: &Path) -> SlackChannel {
    SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_workspace_dir(workspace.to_path_buf())
    .with_api_base_url(server.uri())
}

#[tokio::test]
async fn send_uploads_text_and_outbound_attachment_via_slack_external_flow() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let attachment_path = tmp.path().join("report.txt");
    tokio::fs::write(&attachment_path, b"report-bytes")
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1710000000.000100",
        })))
        .expect(1)
        .mount(&server)
        .await;
    mock_slack_upload_flow(
        &server,
        "F_TEXT",
        "/upload/text",
        200,
        serde_json::json!({"ok": true}),
    )
    .await;

    let ch = test_slack_channel(&server, tmp.path());
    let mut msg = SendMessage::new(format!("Done [FILE:{}]", attachment_path.display()), "C123");
    msg.thread_ts = Some("1709999999.000001".into());

    SlackChannel::send(&ch, &msg).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let post = requests
        .iter()
        .find(|req| req.url.path() == "/chat.postMessage")
        .expect("chat.postMessage should be called");
    let post_body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(post_body["channel"], "C123");
    assert_eq!(post_body["thread_ts"], "1709999999.000001");
    assert_eq!(post_body["text"], "Done");

    let get_upload = requests
        .iter()
        .find(|req| req.url.path() == "/files.getUploadURLExternal")
        .expect("getUploadURLExternal should be called");
    let get_upload_body = String::from_utf8_lossy(&get_upload.body);
    assert!(get_upload_body.contains("filename=report.txt"));
    assert!(get_upload_body.contains("length=12"));

    let upload = requests
        .iter()
        .find(|req| req.url.path() == "/upload/text")
        .expect("byte upload should be called");
    assert_eq!(upload.body.as_slice(), b"report-bytes");

    let complete = requests
        .iter()
        .find(|req| req.url.path() == "/files.completeUploadExternal")
        .expect("completeUploadExternal should be called");
    let complete_body: serde_json::Value = serde_json::from_slice(&complete.body).unwrap();
    assert_eq!(complete_body["channel_id"], "C123");
    assert_eq!(complete_body["thread_ts"], "1709999999.000001");
    assert_eq!(complete_body["files"][0]["id"], "F_TEXT");
    assert_eq!(complete_body["files"][0]["title"], "report.txt");
}

#[tokio::test]
async fn send_uploads_attachment_only_message_without_chat_post() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let attachment_path = tmp.path().join("only.txt");
    tokio::fs::write(&attachment_path, b"only-bytes")
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(500).set_body_string("must-not-call"))
        .expect(0)
        .mount(&server)
        .await;
    mock_slack_upload_flow(
        &server,
        "F_ONLY",
        "/upload/only",
        200,
        serde_json::json!({"ok": true}),
    )
    .await;

    let ch = test_slack_channel(&server, tmp.path());
    let msg = SendMessage::new(format!("[FILE:{}]", attachment_path.display()), "C123");

    SlackChannel::send(&ch, &msg).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|req| req.url.path() != "/chat.postMessage"),
        "attachment-only sends must skip chat.postMessage"
    );
    assert!(
        requests
            .iter()
            .any(|req| req.url.path() == "/files.completeUploadExternal"),
        "attachment-only sends must still complete file upload"
    );
}

#[tokio::test]
async fn send_returns_error_when_slack_complete_upload_fails() {
    use wiremock::MockServer;

    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let attachment_path = tmp.path().join("fail.txt");
    tokio::fs::write(&attachment_path, b"fail-bytes")
        .await
        .unwrap();

    mock_slack_upload_flow(
        &server,
        "F_FAIL",
        "/upload/fail",
        200,
        serde_json::json!({"ok": false, "error": "complete_failed"}),
    )
    .await;

    let ch = test_slack_channel(&server, tmp.path());
    let msg = SendMessage::new(format!("[FILE:{}]", attachment_path.display()), "C123");

    let err = SlackChannel::send(&ch, &msg)
        .await
        .expect_err("Slack completion failure should propagate");
    assert!(
        err.to_string()
            .contains("files.completeUploadExternal failed: complete_failed"),
        "unexpected error: {err}"
    );
}

#[test]
fn ensure_file_extension_appends_when_missing() {
    assert_eq!(
        SlackChannel::ensure_file_extension("capture", "png"),
        "capture.png"
    );
    assert_eq!(
        SlackChannel::ensure_file_extension("capture.jpeg", "png"),
        "capture.jpeg"
    );
}

#[test]
fn is_allowed_slack_media_hostname_matches_suffixes() {
    assert!(SlackChannel::is_allowed_slack_media_hostname(
        "files.slack.com"
    ));
    assert!(SlackChannel::is_allowed_slack_media_hostname(
        "downloads.slack-edge.com"
    ));
    assert!(SlackChannel::is_allowed_slack_media_hostname(
        "foo.slack-files.com"
    ));
    assert!(!SlackChannel::is_allowed_slack_media_hostname(
        "example.com"
    ));
}

#[test]
fn validate_slack_private_file_url_rejects_invalid_schemes_and_hosts() {
    assert!(SlackChannel::validate_slack_private_file_url("https://files.slack.com/f").is_some());
    assert!(SlackChannel::validate_slack_private_file_url("http://files.slack.com/f").is_none());
    assert!(SlackChannel::validate_slack_private_file_url("https://example.com/f").is_none());
    assert!(SlackChannel::validate_slack_private_file_url("not a url").is_none());
}

#[test]
fn resolve_https_redirect_target_enforces_https() {
    let base = reqwest::Url::parse("https://files.slack.com/path/file").unwrap();
    let ok = SlackChannel::resolve_https_redirect_target(&base, "/next");
    assert_eq!(
        ok.as_ref().map(|url| url.as_str()),
        Some("https://files.slack.com/next")
    );

    let rejected =
        SlackChannel::resolve_https_redirect_target(&base, "http://files.slack.com/next");
    assert!(rejected.is_none());

    let rejected_host =
        SlackChannel::resolve_https_redirect_target(&base, "https://example.com/next");
    assert!(rejected_host.is_none());
}

#[test]
fn redact_slack_url_hides_query_fragments() {
    let url = reqwest::Url::parse(
        "https://files.slack.com/files-pri/T1/F2/wow.png?token=secret#fragment",
    )
    .unwrap();
    let redacted = SlackChannel::redact_slack_url(&url);
    assert_eq!(redacted, "files.slack.com/.../wow.png");
    assert!(!redacted.contains('?'));
    assert!(!redacted.contains("token="));
    assert!(!redacted.contains('#'));
}

#[test]
fn redact_redirect_location_keeps_only_relative_tail() {
    let redacted = SlackChannel::redact_redirect_location("/files-pri/T1/F2/wow.png?token=secret");
    assert_eq!(redacted, "relative/.../wow.png");
    assert!(!redacted.contains("token="));
}

#[tokio::test]
async fn resolve_workspace_attachment_output_path_stays_in_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let output =
        SlackChannel::resolve_workspace_attachment_output_path(workspace.path(), "capture.png")
            .await
            .unwrap();

    let root = tokio::fs::canonicalize(workspace.path()).await.unwrap();
    assert!(output.starts_with(&root));
    assert!(output.to_string_lossy().contains("slack_files"));
}

#[tokio::test]
async fn persist_image_attachment_writes_bytes_without_part_leftovers() {
    let workspace = tempfile::tempdir().unwrap();
    let channel = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    )
    .with_workspace_dir(workspace.path().to_path_buf());
    let file = serde_json::json!({"id":"F1","name":"wow.png"});
    let png_bytes = vec![
        0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0x00, 0x01, 0x02, 0x03,
    ];

    let output = channel
        .persist_image_attachment(&file, "wow.png", "image/png", &png_bytes)
        .await
        .expect("attachment path");
    let stored = tokio::fs::read(&output).await.expect("stored bytes");
    assert_eq!(stored, png_bytes);

    let save_dir = output.parent().unwrap();
    let mut entries = tokio::fs::read_dir(save_dir).await.unwrap();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.ends_with(".part"),
            "unexpected temp artifact left behind: {name}"
        );
    }
}

#[test]
fn evaluate_health_enforces_socket_mode_probe_when_enabled() {
    assert!(!SlackChannel::evaluate_health(false, false, true));
    assert!(!SlackChannel::evaluate_health(false, true, true));
    assert!(SlackChannel::evaluate_health(true, false, false));
    assert!(SlackChannel::evaluate_health(true, false, true));
    assert!(!SlackChannel::evaluate_health(true, true, false));
    assert!(SlackChannel::evaluate_health(true, true, true));
}

#[test]
fn slack_api_call_succeeded_requires_ok_true_in_body() {
    assert!(!SlackChannel::slack_api_call_succeeded(
        reqwest::StatusCode::OK,
        r#"{"ok":false,"error":"invalid_auth"}"#
    ));
}

#[test]
fn slack_api_call_succeeded_accepts_ok_true() {
    assert!(SlackChannel::slack_api_call_succeeded(
        reqwest::StatusCode::OK,
        r#"{"ok":true}"#
    ));
}

#[test]
fn specific_allowlist_filters() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["U111".into(), "U222".into()]),
    );
    assert!(ch.is_user_allowed("U111"));
    assert!(ch.is_user_allowed("U222"));
    assert!(!ch.is_user_allowed("U333"));
}

#[test]
fn allowlist_exact_match_not_substring() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["U111".into()]),
    );
    assert!(!ch.is_user_allowed("U1111"));
    assert!(!ch.is_user_allowed("U11"));
}

#[test]
fn allowlist_empty_user_id() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["U111".into()]),
    );
    assert!(!ch.is_user_allowed(""));
}

#[test]
fn allowlist_case_sensitive() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["U111".into()]),
    );
    assert!(ch.is_user_allowed("U111"));
    assert!(!ch.is_user_allowed("u111"));
}

#[test]
fn allowlist_wildcard_and_specific() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(|| vec!["U111".into(), "*".into()]),
    );
    assert!(ch.is_user_allowed("U111"));
    assert!(ch.is_user_allowed("anyone"));
}

// ── Message ID edge cases ─────────────────────────────────────

#[test]
fn slack_message_id_format_includes_channel_and_ts() {
    // Verify that message IDs follow the format: slack_{channel_id}_{ts}
    let ts = "1234567890.123456";
    let channel_id = "C12345";
    let expected_id = format!("slack_{channel_id}_{ts}");
    assert_eq!(expected_id, "slack_C12345_1234567890.123456");
}

#[test]
fn slack_message_id_is_deterministic() {
    // Same channel_id + same ts = same ID (prevents duplicates after restart)
    let ts = "1234567890.123456";
    let channel_id = "C12345";
    let id1 = format!("slack_{channel_id}_{ts}");
    let id2 = format!("slack_{channel_id}_{ts}");
    assert_eq!(id1, id2);
}

#[test]
fn slack_message_id_different_ts_different_id() {
    // Different timestamps produce different IDs
    let channel_id = "C12345";
    let id1 = format!("slack_{channel_id}_1234567890.123456");
    let id2 = format!("slack_{channel_id}_1234567890.123457");
    assert_ne!(id1, id2);
}

#[test]
fn slack_message_id_different_channel_different_id() {
    // Different channels produce different IDs even with same ts
    let ts = "1234567890.123456";
    let id1 = format!("slack_C12345_{ts}");
    let id2 = format!("slack_C67890_{ts}");
    assert_ne!(id1, id2);
}

#[test]
fn slack_message_id_no_uuid_randomness() {
    // Verify format doesn't contain random UUID components
    let ts = "1234567890.123456";
    let channel_id = "C12345";
    let id = format!("slack_{channel_id}_{ts}");
    assert!(!id.contains('-')); // No UUID dashes
    assert!(id.starts_with("slack_"));
}

#[test]
fn inbound_thread_ts_prefers_explicit_thread_ts() {
    let msg = serde_json::json!({
        "ts": "123.002",
        "thread_ts": "123.001"
    });

    let thread_ts = SlackChannel::inbound_thread_ts(&msg, "123.002");
    assert_eq!(thread_ts.as_deref(), Some("123.001"));
}

#[test]
fn inbound_thread_ts_falls_back_to_ts() {
    let msg = serde_json::json!({
        "ts": "123.001"
    });

    let thread_ts = SlackChannel::inbound_thread_ts(&msg, "123.001");
    assert_eq!(thread_ts.as_deref(), Some("123.001"));
}

#[test]
fn inbound_thread_ts_none_when_ts_missing() {
    let msg = serde_json::json!({});

    let thread_ts = SlackChannel::inbound_thread_ts(&msg, "");
    assert_eq!(thread_ts, None);
}

#[test]
fn ensure_poll_cursor_bootstraps_new_channel() {
    let mut cursors = HashMap::new();
    let now_ts = "1700000000.123456";

    let cursor = SlackChannel::ensure_poll_cursor(&mut cursors, "C123", now_ts);
    assert_eq!(cursor, now_ts);
    assert_eq!(cursors.get("C123").map(String::as_str), Some(now_ts));
}

#[test]
fn ensure_poll_cursor_keeps_existing_cursor() {
    let mut cursors = HashMap::from([("C123".to_string(), "1700000000.000001".to_string())]);
    let cursor = SlackChannel::ensure_poll_cursor(&mut cursors, "C123", "9999999999.999999");

    assert_eq!(cursor, "1700000000.000001");
    assert_eq!(
        cursors.get("C123").map(String::as_str),
        Some("1700000000.000001")
    );
}

#[test]
fn parse_retry_after_value_accepts_integer_seconds() {
    assert_eq!(SlackChannel::parse_retry_after_value("30"), Some(30));
}

#[test]
fn parse_retry_after_value_accepts_decimal_seconds() {
    assert_eq!(SlackChannel::parse_retry_after_value("2.9"), Some(2));
}

#[test]
fn parse_retry_after_value_rejects_non_numeric_values() {
    assert_eq!(SlackChannel::parse_retry_after_value("later"), None);
    assert_eq!(SlackChannel::parse_retry_after_value(""), None);
}

#[test]
fn parse_retry_after_secs_reads_header_value() {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "45".parse().unwrap());
    assert_eq!(SlackChannel::parse_retry_after_secs(&headers), Some(45));
}

#[test]
fn compute_retry_delay_applies_backoff_and_jitter_with_cap() {
    let delay = SlackChannel::compute_retry_delay(30, 3, 250);
    assert_eq!(delay, Duration::from_secs(120) + Duration::from_millis(250));
}

// ── Thread reply handling ────────────────────────────────────

#[test]
fn extract_active_threads_finds_thread_parents_with_replies() {
    let messages = vec![
        serde_json::json!({
            "ts": "100.000",
            "thread_ts": "100.000",
            "reply_count": 3,
            "latest_reply": "103.000"
        }),
        serde_json::json!({
            "ts": "200.000",
            "text": "no thread"
        }),
        serde_json::json!({
            "ts": "300.000",
            "thread_ts": "300.000",
            "reply_count": 0
        }),
    ];

    let threads = SlackChannel::extract_active_threads(&messages);
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].0, "100.000");
    assert_eq!(threads[0].1, "103.000");
}

#[test]
fn extract_active_threads_ignores_reply_messages() {
    // A reply message has ts != thread_ts; it should not be treated as a thread parent.
    let messages = vec![serde_json::json!({
        "ts": "101.000",
        "thread_ts": "100.000",
        "text": "reply in thread"
    })];

    let threads = SlackChannel::extract_active_threads(&messages);
    assert!(threads.is_empty());
}

#[test]
fn extract_active_threads_uses_thread_ts_as_fallback_latest_reply() {
    let messages = vec![serde_json::json!({
        "ts": "100.000",
        "thread_ts": "100.000",
        "reply_count": 1
    })];

    let threads = SlackChannel::extract_active_threads(&messages);
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].1, "100.000");
}

#[test]
fn evict_stale_threads_removes_expired_entries() {
    let mut threads: HashMap<String, (String, String, Instant)> = HashMap::new();
    let old = Instant::now()
        .checked_sub(Duration::from_secs(SLACK_POLL_THREAD_EXPIRE_SECS + 1))
        .unwrap();
    threads.insert(
        "old.thread".to_string(),
        ("C1".to_string(), "old.reply".to_string(), old),
    );
    threads.insert(
        "new.thread".to_string(),
        ("C1".to_string(), "new.reply".to_string(), Instant::now()),
    );

    SlackChannel::evict_stale_threads(&mut threads, Instant::now());
    assert_eq!(threads.len(), 1);
    assert!(threads.contains_key("new.thread"));
}

#[test]
fn evict_stale_threads_trims_excess_by_oldest_key() {
    let mut threads: HashMap<String, (String, String, Instant)> = HashMap::new();
    let now = Instant::now();
    for i in 0..(SLACK_POLL_ACTIVE_THREAD_MAX + 5) {
        threads.insert(
            format!("{i:06}.000"),
            ("C1".to_string(), format!("{i:06}.001"), now),
        );
    }

    SlackChannel::evict_stale_threads(&mut threads, now);
    assert_eq!(threads.len(), SLACK_POLL_ACTIVE_THREAD_MAX);
}

#[test]
fn is_supported_message_subtype_rejects_message_replied() {
    // message_replied is a parent-level notification, not an actual reply.
    assert!(!SlackChannel::is_supported_message_subtype(Some(
        "message_replied"
    )));
}

#[test]
fn extract_slack_ts_from_standard_message_id() {
    assert_eq!(
        extract_slack_ts("slack_C1234567890_1234567890.123456"),
        "1234567890.123456"
    );
}

#[test]
fn extract_slack_ts_from_raw_ts_passthrough() {
    assert_eq!(extract_slack_ts("1234567890.123456"), "1234567890.123456");
}

#[test]
fn extract_slack_ts_from_unprefixed_id() {
    assert_eq!(extract_slack_ts("unknown_format"), "unknown_format");
}

#[test]
fn unicode_emoji_maps_to_slack_eyes() {
    assert_eq!(unicode_emoji_to_slack_name("\u{1F440}"), "eyes");
}

#[test]
fn unicode_emoji_maps_to_slack_check_mark() {
    assert_eq!(unicode_emoji_to_slack_name("\u{2705}"), "white_check_mark");
}

#[test]
fn unicode_emoji_maps_to_slack_warning() {
    assert_eq!(unicode_emoji_to_slack_name("\u{26A0}\u{FE0F}"), "warning");
    assert_eq!(unicode_emoji_to_slack_name("\u{26A0}"), "warning");
}

#[test]
fn unicode_emoji_colon_wrapped_passthrough() {
    assert_eq!(
        unicode_emoji_to_slack_name(":custom_emoji:"),
        "custom_emoji"
    );
}

#[test]
fn inbound_thread_ts_on_thread_reply_uses_thread_ts() {
    let reply = serde_json::json!({
        "ts": "200.000",
        "thread_ts": "100.000",
        "text": "a thread reply"
    });
    let thread_ts = SlackChannel::inbound_thread_ts(&reply, "200.000");
    assert_eq!(thread_ts.as_deref(), Some("100.000"));
}

#[test]
fn inbound_thread_ts_genuine_only_returns_none_for_top_level() {
    // Top-level messages don't have thread_ts in Slack's API.
    let msg = serde_json::json!({
        "ts": "100.000",
        "text": "hello"
    });
    assert_eq!(SlackChannel::inbound_thread_ts_genuine_only(&msg), None);
}

#[test]
fn inbound_thread_ts_genuine_only_returns_thread_ts_for_replies() {
    // Thread replies have thread_ts pointing to the parent message.
    let reply = serde_json::json!({
        "ts": "200.000",
        "thread_ts": "100.000",
        "text": "a reply"
    });
    assert_eq!(
        SlackChannel::inbound_thread_ts_genuine_only(&reply).as_deref(),
        Some("100.000")
    );
}

#[test]
fn session_key_stable_without_thread_replies() {
    // When thread_replies=false, top-level messages from the same user should
    // produce the same conversation_history_key (thread_ts=None).
    use zeroclaw_api::channel::ChannelMessage;

    let make_msg = |ts: &str| ChannelMessage {
        id: format!("slack_C123_{ts}"),
        sender: "U_alice".into(),
        reply_target: "C123".into(),
        content: "text".into(),
        channel: "slack".into(),
        channel_alias: None,
        timestamp: 0,
        thread_ts: None, // thread_replies=false → no fallback to ts
        interruption_scope_id: None,
        attachments: vec![],
        subject: None,

        ..Default::default()
    };

    let msg1 = make_msg("100.000");
    let msg2 = make_msg("200.000");

    let key1 = crate::util::conversation_history_key(&msg1);
    let key2 = crate::util::conversation_history_key(&msg2);
    assert_eq!(key1, key2, "session key should be stable across messages");
}

#[test]
fn session_key_varies_with_thread_replies() {
    // When thread_replies=true, top-level messages get thread_ts=Some(ts),
    // giving each its own session key (thread isolation).
    use zeroclaw_api::channel::ChannelMessage;

    let make_msg = |ts: &str| ChannelMessage {
        id: format!("slack_C123_{ts}"),
        sender: "U_alice".into(),
        reply_target: "C123".into(),
        content: "text".into(),
        channel: "slack".into(),
        channel_alias: None,
        timestamp: 0,
        thread_ts: Some(ts.to_string()), // thread_replies=true → ts as thread_ts
        interruption_scope_id: None,
        attachments: vec![],
        subject: None,

        ..Default::default()
    };

    let msg1 = make_msg("100.000");
    let msg2 = make_msg("200.000");

    let key1 = crate::util::conversation_history_key(&msg1);
    let key2 = crate::util::conversation_history_key(&msg2);
    assert_ne!(key1, key2, "session key should differ per thread");
}

#[test]
fn slack_send_uses_markdown_blocks() {
    let msg = SendMessage::new("**bold** and _italic_", "C123");
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );

    // Build the same JSON body that send() would construct.
    let mut body = serde_json::json!({
        "channel": msg.recipient,
        "text": msg.content
    });
    if msg.content.len() <= SLACK_MARKDOWN_BLOCK_MAX_CHARS {
        body["blocks"] = serde_json::json!([{
            "type": "markdown",
            "text": msg.content
        }]);
    }

    // Verify blocks are present with correct structure.
    let blocks = body["blocks"]
        .as_array()
        .expect("blocks should be an array");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "markdown");
    assert_eq!(blocks[0]["text"], msg.content);
    // text field kept as plaintext fallback.
    assert_eq!(body["text"], msg.content);
    // Suppress unused variable warning.
    let _ = ch.name();
}

#[test]
fn slack_send_skips_markdown_blocks_for_long_content() {
    let long_content = "x".repeat(SLACK_MARKDOWN_BLOCK_MAX_CHARS + 1);
    let msg = SendMessage::new(long_content.clone(), "C123");

    let mut body = serde_json::json!({
        "channel": msg.recipient,
        "text": msg.content
    });
    if msg.content.len() <= SLACK_MARKDOWN_BLOCK_MAX_CHARS {
        body["blocks"] = serde_json::json!([{
            "type": "markdown",
            "text": msg.content
        }]);
    }

    assert!(
        body.get("blocks").is_none(),
        "blocks should not be set for oversized content"
    );
}

#[tokio::test]
async fn start_typing_requires_thread_context() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    // No thread_ts tracked for "C999" — start_typing should be a no-op (Ok).
    let result = ch.start_typing("C999").await;
    assert!(
        result.is_ok(),
        "start_typing should succeed as no-op without thread context"
    );
}

#[test]
fn assistant_thread_tracking() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );

    // Initially empty.
    {
        let map = ch.active_assistant_thread.lock().unwrap();
        assert!(map.is_empty());
    }

    // Simulate storing a thread_ts (as listen_socket_mode would).
    {
        let mut map = ch.active_assistant_thread.lock().unwrap();
        map.insert("C123".to_string(), "1741234567.000100".to_string());
    }

    // Verify retrieval.
    {
        let map = ch.active_assistant_thread.lock().unwrap();
        assert_eq!(map.get("C123"), Some(&"1741234567.000100".to_string()),);
        assert_eq!(map.get("C999"), None);
    }
}

#[test]
fn pending_approvals_map_is_initially_empty() {
    let ch = SlackChannel::new(
        "xoxb-token".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    let map = ch.pending_approvals.try_lock().unwrap();
    assert!(map.is_empty());
}

#[test]
fn approval_timeout_defaults_to_300_and_is_overridable() {
    let ch = SlackChannel::new(
        "xoxb-token".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    assert_eq!(ch.approval_timeout_secs, 300);
    let ch = ch.with_approval_timeout_secs(90);
    assert_eq!(ch.approval_timeout_secs, 90);
}

#[tokio::test]
async fn pending_approval_oneshot_delivers_response() {
    let ch = SlackChannel::new(
        "xoxb-token".into(),
        None,
        vec![],
        "slack_test_alias",
        Arc::new(Vec::new),
    );
    let (tx, rx) = oneshot::channel();
    ch.pending_approvals
        .lock()
        .await
        .insert("abc123".to_string(), tx);
    let sender = ch.pending_approvals.lock().await.remove("abc123").unwrap();
    sender.send(ChannelApprovalResponse::AlwaysApprove).unwrap();
    assert_eq!(rx.await.unwrap(), ChannelApprovalResponse::AlwaysApprove);
}

#[test]
fn approval_block_action_parsed_correctly() {
    let envelope = serde_json::json!({
        "payload": {
            "type": "block_actions",
            "actions": [{ "action_id": "approval_abc123_approve" }]
        }
    });
    let (token, response) = SlackChannel::try_parse_approval_block_action(&envelope).unwrap();
    assert_eq!(token, "abc123");
    assert_eq!(response, ChannelApprovalResponse::Approve);
}

#[test]
fn approval_block_action_deny_parsed() {
    let envelope = serde_json::json!({
        "payload": {
            "type": "block_actions",
            "actions": [{ "action_id": "approval_xz9q1w_deny" }]
        }
    });
    let (token, response) = SlackChannel::try_parse_approval_block_action(&envelope).unwrap();
    assert_eq!(token, "xz9q1w");
    assert_eq!(response, ChannelApprovalResponse::Deny);
}

#[test]
fn approval_block_action_non_approval_returns_none() {
    let envelope = serde_json::json!({
        "payload": {
            "type": "block_actions",
            "actions": [{ "action_id": "zeroclaw_config_provider", "selected_option": { "value": "anthropic" } }]
        }
    });
    assert!(SlackChannel::try_parse_approval_block_action(&envelope).is_none());
}

// --- Thread-context backfill tests ---

#[test]
fn thread_backfill_precheck_skips_non_replies() {
    let seen = Mutex::new(HashSet::new());

    let top_level = serde_json::json!({
        "ts": "1700000010.000100",
        "text": "hi bot",
        "user": "U_USER",
    });
    assert!(SlackChannel::precheck_thread_backfill(&top_level, &seen).is_none());

    let parent = serde_json::json!({
        "ts": "1700000000.000001",
        "thread_ts": "1700000000.000001",
        "text": "thread starts here",
        "user": "U_USER",
    });
    assert!(SlackChannel::precheck_thread_backfill(&parent, &seen).is_none());
}

#[test]
fn thread_backfill_filter_drops_trigger_subtype_userless_and_non_allow_listed() {
    let messages = vec![
        // Parent message from allow-listed user — keep.
        serde_json::json!({"ts": "T_PARENT", "user": "U_USER", "text": "parent"}),
        // Earlier reply from allow-listed user — keep.
        serde_json::json!({"ts": "T_R1", "thread_ts": "T_PARENT", "user": "U_USER", "text": "first"}),
        // System message (`channel_join`) — must be dropped by subtype.
        serde_json::json!({
            "ts": "T_R2",
            "thread_ts": "T_PARENT",
            "subtype": "channel_join",
            "user": "U_USER",
            "text": "joined",
        }),
        // Reply from non-allow-listed user — must be counted as a drop.
        serde_json::json!({"ts": "T_R3", "thread_ts": "T_PARENT", "user": "U_BAD", "text": "filtered"}),
        // Bot's own past reply — must pass through (useful self-context).
        serde_json::json!({"ts": "T_R4", "thread_ts": "T_PARENT", "user": "U_BOT", "text": "bot turn"}),
        serde_json::json!({"ts": "T_R5", "thread_ts": "T_PARENT", "text": "from a webhook"}),
        // Same situation with a `bot_id` instead of `user` (some
        // integration messages carry `bot_id` instead of `user`).
        // Filter only inspects `user`, so `bot_id`-only messages
        // also fall under the userless drop.
        serde_json::json!({"ts": "T_R6", "thread_ts": "T_PARENT", "bot_id": "B999", "text": "from a bot integration"}),
        // The triggering reply itself — must be dropped to avoid
        // duplication in the agent payload.
        serde_json::json!({"ts": "T_TRIGGER", "thread_ts": "T_PARENT", "user": "U_USER", "text": "hi bot"}),
    ];

    let (allowed, dropped) =
        SlackChannel::filter_backfill_messages(&messages, "T_TRIGGER", "U_BOT", |user| {
            user == "U_USER"
        });

    let kept_ts: Vec<&str> = allowed
        .iter()
        .map(|m| m.get("ts").and_then(|v| v.as_str()).unwrap_or_default())
        .collect();
    assert_eq!(
        kept_ts,
        vec!["T_PARENT", "T_R1", "T_R4"],
        "trigger, subtype, userless, and non-allow-listed messages must be dropped",
    );
    assert_eq!(
        dropped, 3,
        "non-allow-listed and userless messages all count toward the gap marker",
    );
}

#[test]
fn thread_backfill_compose_renders_allow_list_gap_marker() {
    let lines = vec![
        "- alice: first".to_string(),
        "- bob: second".to_string(),
        "- alice: third".to_string(),
    ];
    let block =
        SlackChannel::compose_thread_backfill_block(lines, 2, 0).expect("block should be produced");
    assert!(block.starts_with("[Thread context]\n"));
    assert!(block.contains("… 2 messages from non-allow-listed users omitted …"));
    assert!(block.contains("- alice: first"));
    assert!(block.contains("- bob: second"));
    assert!(block.contains("- alice: third"));
    // Reply-cap marker must NOT appear when reply_cap_omitted == 0.
    assert!(!block.contains("earlier thread messages omitted"));
}

#[test]
fn thread_backfill_compose_with_only_dropped_senders() {
    let block = SlackChannel::compose_thread_backfill_block(Vec::new(), 4, 0)
        .expect("block should still surface the gap signal");
    assert!(block.starts_with("[Thread context]\n"));
    assert!(block.contains("… 4 messages from non-allow-listed users omitted …"));
    // No rendered message lines.
    assert!(!block.contains("- "));
}

#[test]
fn thread_backfill_compose_renders_reply_cap_marker() {
    let lines = (0..SLACK_PERMALINK_THREAD_MAX_REPLIES)
        .map(|i| format!("- alice: message {i}"))
        .collect::<Vec<_>>();
    let block =
        SlackChannel::compose_thread_backfill_block(lines, 0, 5).expect("block should be produced");
    assert!(block.starts_with("[Thread context]\n"));
    assert!(block.contains("… 5 earlier thread messages omitted …"));
    // Allow-list marker must NOT appear when dropped_by_allow_list == 0.
    assert!(!block.contains("non-allow-listed"));
}

#[test]
fn thread_backfill_compose_truncates_long_text() {
    let huge = "x".repeat(SLACK_PERMALINK_TEXT_MAX_CHARS + 1000);
    let lines = vec![format!("- alice: {huge}")];
    let block =
        SlackChannel::compose_thread_backfill_block(lines, 0, 0).expect("block should be produced");
    assert!(block.ends_with("…[truncated]"));
    assert!(block.starts_with("[Thread context]"));
}

#[test]
fn thread_backfill_strip_bot_mentions_removes_self_mention() {
    let line = "- alice: hey <@U_BOT> can you help?";
    let stripped = SlackChannel::strip_bot_mentions(line, "U_BOT");
    assert!(!stripped.contains("<@U_BOT>"));
    assert!(stripped.contains("alice:"));
    assert!(stripped.contains("can you help?"));
}

#[test]
fn thread_backfill_first_reply_backfills_then_subsequent_replies_do_not() {
    let ch = SlackChannel::new(
        "xoxb-fake".into(),
        None,
        vec!["C1".into()],
        "slack_test_alias",
        Arc::new(Vec::new),
    );

    let reply1 = serde_json::json!({
        "ts": "T_REPLY1",
        "thread_ts": "T_PARENT",
        "user": "U_USER",
        "text": "first reply",
    });
    assert_eq!(
        SlackChannel::precheck_thread_backfill(&reply1, &ch.seen_threads).as_deref(),
        Some("T_PARENT"),
        "first forwarded reply must trigger backfill",
    );

    // Simulate a successful render: maybe_render_thread_backfill records
    // on Some(_) only.
    SlackChannel::record_thread_seen(&ch.seen_threads, "T_PARENT");

    let reply2 = serde_json::json!({
        "ts": "T_REPLY2",
        "thread_ts": "T_PARENT",
        "user": "U_USER",
        "text": "second reply",
    });
    assert!(
        SlackChannel::precheck_thread_backfill(&reply2, &ch.seen_threads).is_none(),
        "second reply must not re-backfill",
    );
}

#[test]
fn thread_backfill_block_is_prepended_to_payload() {
    let backfill_block = Some("[Thread context]\n- alice: hi\n- bob: hello".to_string());
    let normalized_text = "<@U_BOT> please summarize".to_string();
    let attachment_blocks: Vec<String> = vec!["[Attachment] report.pdf".to_string()];

    // Mirror the assembly done at the tail of `build_incoming_content`.
    let body = SlackChannel::compose_incoming_content(normalized_text, attachment_blocks)
        .expect("non-empty body");
    let payload = match backfill_block {
        Some(block) => format!("{block}\n\n{body}"),
        None => body,
    };

    assert!(
        payload.starts_with("[Thread context]"),
        "payload must lead with [Thread context], got: {payload:?}",
    );
    let ctx_end = payload.find("\n\n").expect("context separator");
    let after_ctx = &payload[ctx_end + 2..];
    assert!(
        after_ctx.starts_with("<@U_BOT> please summarize"),
        "triggering message must follow the context block, got: {after_ctx:?}",
    );
    assert!(
        payload.contains("[Attachment] report.pdf"),
        "attachment blocks still appended after the message",
    );
    assert!(
        payload.rfind("[Attachment]").unwrap() > payload.rfind("please summarize").unwrap(),
        "attachments must come after the message body",
    );
}

#[test]
fn thread_backfill_fetch_failure_does_not_record_thread_as_seen() {
    let seen = Mutex::new(HashSet::new());

    // Simulate: precheck returned Some, render returned None.
    let block: Option<String> = None;
    if block.is_some() {
        SlackChannel::record_thread_seen(&seen, "T_PARENT");
    }
    assert!(seen.lock().unwrap().is_empty());

    // Sanity: the same flow with a successful render does record.
    let block: Option<String> = Some("[Thread context]\n- alice: hi".to_string());
    if block.is_some() {
        SlackChannel::record_thread_seen(&seen, "T_PARENT");
    }
    assert!(seen.lock().unwrap().contains("T_PARENT"));
}
