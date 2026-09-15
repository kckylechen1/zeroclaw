use super::*;
use tempfile::tempdir;

fn test_wechat_channel_for_api(api_base_url: String, state_dir: &Path) -> WeChatChannel {
    let mut channel = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        None,
        None,
        Some(state_dir.to_path_buf()),
    )
    .unwrap();
    channel.api_base_url = api_base_url;
    *channel.bot_token.write().unwrap() = Some("test-token".into());
    channel
}

#[tokio::test]
async fn send_text_reports_2xx_error_envelope() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/sendmessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ret": -1,
            "errcode": 301,
            "errmsg": "context token expired"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let state = tempdir().unwrap();
    let channel = test_wechat_channel_for_api(server.uri(), state.path());
    let err = channel
        .send_text("recipient", "hello", None)
        .await
        .expect_err("a 2xx iLink error envelope must fail the send");

    let message = err.to_string();
    assert!(message.contains("sendMessage failed"), "{message}");
    assert!(message.contains("errcode=301"), "{message}");
}

#[tokio::test]
async fn send_text_propagates_2xx_body_read_failure() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = zeroclaw_spawn::spawn!(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });

    let state = tempdir().unwrap();
    let channel = test_wechat_channel_for_api(format!("http://{address}"), state.path());
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        channel.send_text("recipient", "hello", None),
    )
    .await
    .expect("the local truncated-body request must complete")
    .expect_err("a truncated 2xx response body must fail the send");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the local truncated-body server must complete")
        .unwrap();

    assert!(
        err.to_string()
            .contains("failed to read sendMessage response body"),
        "{err:#}"
    );
}

#[test]
fn sendmessage_body_error_flags_nonzero_ret() {
    let err = sendmessage_body_error(r#"{"ret":-1,"errmsg":"context token expired"}"#)
        .expect("non-zero ret must be reported as an error");
    assert!(err.contains("ret=-1"), "ret code missing from error: {err}");
    assert!(
        err.contains("context token expired"),
        "errmsg missing from error: {err}"
    );
}

#[test]
fn sendmessage_body_error_flags_nonzero_errcode() {
    let err = sendmessage_body_error(r#"{"ret":0,"errcode":301,"errmsg":"session expired"}"#)
        .expect("non-zero errcode must be reported as an error");
    assert!(err.contains("errcode=301"), "errcode missing: {err}");
}

#[test]
fn sendmessage_body_error_accepts_success_envelope() {
    assert_eq!(
        sendmessage_body_error(r#"{"ret":0,"errcode":0,"errmsg":""}"#),
        None
    );
    // Fields absent entirely also means success (defaults are 0).
    assert_eq!(sendmessage_body_error(r#"{"msg_id":"abc"}"#), None);
}

#[test]
fn sendmessage_body_error_preserves_legacy_success_for_empty_or_non_json() {
    // An empty 2xx body was success before this check existed; keep it so.
    assert_eq!(sendmessage_body_error(""), None);
    assert_eq!(sendmessage_body_error("   "), None);
    // A non-JSON 2xx body has no envelope to inspect; do not invent failures.
    assert_eq!(sendmessage_body_error("OK"), None);
}

#[test]
fn wechat_channel_name() {
    let ch = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        None,
        None,
        Some("/tmp/test-wechat".into()),
    )
    .unwrap();
    assert_eq!(ch.name(), "wechat");
}

#[test]
fn has_persisted_login_requires_non_empty_account_token() {
    let temp = tempdir().unwrap();
    let dir = temp.path();

    assert!(!WeChatChannel::has_persisted_login(dir));

    // A token cleared on logout is not a persisted login.
    std::fs::write(dir.join("account.json"), r#"{"token": ""}"#).unwrap();
    assert!(!WeChatChannel::has_persisted_login(dir));

    std::fs::write(
        dir.join("account.json"),
        r#"{"token": "tok_persisted", "account_id": "acct_1"}"#,
    )
    .unwrap();
    assert!(WeChatChannel::has_persisted_login(dir));
}

#[test]
fn clear_persisted_login_removes_state_files_and_is_idempotent() {
    let temp = tempdir().unwrap();
    let dir = temp.path();
    std::fs::write(dir.join("account.json"), r#"{"token": "tok_persisted"}"#).unwrap();
    std::fs::write(dir.join("sync.json"), r#"{"get_updates_buf": "cursor"}"#).unwrap();

    let removed = WeChatChannel::clear_persisted_login(dir).unwrap();
    assert_eq!(removed.len(), 2);
    assert!(!dir.join("account.json").exists());
    assert!(!dir.join("sync.json").exists());
    assert!(!WeChatChannel::has_persisted_login(dir));
    assert!(dir.exists(), "the state directory itself must survive");

    // Relinking an already unpaired channel is a safe no-op.
    let removed = WeChatChannel::clear_persisted_login(dir).unwrap();
    assert!(removed.is_empty());
}

#[test]
fn wechat_channel_rejects_http_api_base_url() {
    let result = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        Some("http://ilink.example.test".into()),
        None,
        Some("/tmp/test-wechat".into()),
    );
    assert!(result.is_err());

    let err = result.err().unwrap();
    assert!(err.to_string().contains("api_base_url must use https://"));
}

#[test]
fn wechat_channel_rejects_http_cdn_base_url() {
    let result = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        None,
        Some("http://cdn.example.test".into()),
        Some("/tmp/test-wechat".into()),
    );
    assert!(result.is_err());

    let err = result.err().unwrap();
    assert!(err.to_string().contains("cdn_base_url must use https://"));
}

#[test]
fn extract_text_from_items_text() {
    let items = vec![serde_json::json!({
        "type": 1,
        "text_item": { "text": "hello world" }
    })];
    assert_eq!(extract_text_from_items(&items), "hello world");
}

#[test]
fn extract_text_from_items_voice() {
    let items = vec![serde_json::json!({
        "type": 3,
        "voice_item": { "text": "voice transcription" }
    })];
    assert_eq!(extract_text_from_items(&items), "voice transcription");
}

#[test]
fn extract_text_from_items_empty() {
    let items = vec![serde_json::json!({
        "type": 2,
        "image_item": {}
    })];
    assert_eq!(extract_text_from_items(&items), "");
}

#[test]
fn extract_bind_code_valid() {
    assert_eq!(
        WeChatChannel::extract_bind_code("/bind ABC123"),
        Some("ABC123")
    );
}

#[test]
fn extract_bind_code_no_code() {
    assert_eq!(WeChatChannel::extract_bind_code("/bind"), None);
}

#[test]
fn extract_bind_code_wrong_command() {
    assert_eq!(WeChatChannel::extract_bind_code("/start"), None);
}

#[test]
fn is_user_allowed_wildcard() {
    let ch = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        None,
        None,
        Some("/tmp/test-wechat".into()),
    )
    .unwrap();
    assert!(ch.is_user_allowed("anyone@im.wechat"));
}

#[test]
fn is_user_allowed_specific() {
    let ch = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["user1@im.wechat".into()]),
        None,
        None,
        Some("/tmp/test-wechat".into()),
    )
    .unwrap();
    assert!(ch.is_user_allowed("user1@im.wechat"));
    assert!(!ch.is_user_allowed("user2@im.wechat"));
}

#[tokio::test]
async fn persist_allowed_identity_without_handle_warns_and_returns_ok() {
    let ch = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(Vec::new),
        None,
        None,
        Some("/tmp/test-wechat".into()),
    )
    .unwrap();
    // No `.with_persistence(...)` wired — should not panic, returns Ok(()).
    let result = ch.persist_allowed_identity("user_xyz@im.wechat").await;
    assert!(result.is_ok());
}

#[test]
fn random_wechat_uin_is_base64() {
    let uin = random_wechat_uin();
    assert!(!uin.is_empty());
    // Should be valid base64
    assert!(base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &uin).is_ok());
}

#[test]
fn extract_text_with_ref_msg() {
    let items = vec![serde_json::json!({
        "type": 1,
        "text_item": { "text": "reply text" },
        "ref_msg": { "title": "original message" }
    })];
    assert_eq!(
        extract_text_from_items(&items),
        "[引用: original message]\nreply text"
    );
}

#[test]
fn parse_attachment_markers_extracts_multiple_types() {
    let message = "See this\n[IMAGE:/tmp/a.png]\n[DOCUMENT:https://example.com/a.pdf]";
    let (cleaned, attachments) = parse_attachment_markers(message);

    assert_eq!(cleaned, "See this");
    assert_eq!(attachments.len(), 2);
    assert_eq!(attachments[0].kind, WeChatAttachmentKind::Image);
    assert_eq!(attachments[0].target, "/tmp/a.png");
    assert_eq!(attachments[1].kind, WeChatAttachmentKind::Document);
    assert_eq!(attachments[1].target, "https://example.com/a.pdf");
}

#[test]
fn parse_attachment_markers_keeps_invalid_marker_text() {
    let message = "See [UNKNOWN:/tmp/a.bin]";
    let (cleaned, attachments) = parse_attachment_markers(message);
    assert_eq!(cleaned, message);
    assert!(attachments.is_empty());
}

#[test]
fn parse_path_only_attachment_detects_existing_file() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("photo.png");
    std::fs::write(&image_path, b"png").unwrap();

    let parsed = parse_path_only_attachment(image_path.to_string_lossy().as_ref())
        .expect("expected attachment");
    assert_eq!(parsed.kind, WeChatAttachmentKind::Image);
    assert_eq!(parsed.target, image_path.to_string_lossy());
}

#[test]
fn parse_path_only_attachment_rejects_sentence_text() {
    assert!(parse_path_only_attachment("saved to /tmp/photo.png").is_none());
}

#[test]
fn format_attachment_content_uses_image_marker_for_images() {
    let path = PathBuf::from("/tmp/workspace/photo.png");
    assert_eq!(
        format_attachment_content(WeChatAttachmentKind::Image, "photo.png", &path),
        "[IMAGE:/tmp/workspace/photo.png]"
    );
}

#[test]
fn format_attachment_content_uses_document_marker_for_non_images() {
    let path = PathBuf::from("/tmp/workspace/report.pdf");
    assert_eq!(
        format_attachment_content(WeChatAttachmentKind::Document, "report.pdf", &path),
        "[Document: report.pdf] /tmp/workspace/report.pdf"
    );
}

#[test]
fn unavailable_attachment_notice_matches_inbound_status_markers() {
    assert_eq!(
        format_unavailable_attachment(WeChatAttachmentKind::Image),
        "[Image: unavailable]"
    );
    assert_eq!(
        format_unavailable_attachment(WeChatAttachmentKind::Document),
        "[Document: unavailable]"
    );
    assert_eq!(
        format_unavailable_attachment(WeChatAttachmentKind::Audio),
        "[Audio: unavailable]"
    );
}

fn test_wechat_channel_with_workspace(workspace_dir: &Path) -> WeChatChannel {
    WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        None,
        None,
        Some(workspace_dir.join("state")),
    )
    .unwrap()
    .with_workspace_dir(workspace_dir.to_path_buf())
}

#[test]
fn resolve_local_attachment_path_requires_workspace_dir() {
    let temp = tempdir().unwrap();
    let ch = WeChatChannel::new(
        "wechat_test_alias",
        Arc::new(|| vec!["*".into()]),
        None,
        None,
        Some(temp.path().join("state")),
    )
    .unwrap();
    let err = ch.resolve_local_attachment_path("photo.png").unwrap_err();
    assert!(
        err.to_string()
            .contains("workspace directory is not configured"),
        "got: {err}"
    );
}

#[test]
fn resolve_local_attachment_path_accepts_relative_workspace_path() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    assert_eq!(
        ch.resolve_local_attachment_path("photo.png").unwrap(),
        workspace.join("photo.png")
    );
}

#[test]
fn resolve_local_attachment_path_accepts_workspace_prefix() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    assert_eq!(
        ch.resolve_local_attachment_path("/workspace/photo.png")
            .unwrap(),
        workspace.join("photo.png")
    );
}

#[test]
fn resolve_local_attachment_path_accepts_file_uri_with_workspace_prefix() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    assert_eq!(
        ch.resolve_local_attachment_path("file:///workspace/photo.png")
            .unwrap(),
        workspace.join("photo.png")
    );
}

#[test]
fn resolve_local_attachment_path_accepts_absolute_path_inside_workspace() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    let file = workspace.join("photo.png");
    assert_eq!(
        ch.resolve_local_attachment_path(file.to_str().unwrap())
            .unwrap(),
        file
    );
}

#[test]
fn resolve_local_attachment_path_normalizes_within_workspace() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    assert_eq!(
        ch.resolve_local_attachment_path("/workspace/sub/../photo.png")
            .unwrap(),
        workspace.join("photo.png")
    );
}

#[test]
fn resolve_local_attachment_path_rejects_dotdot_escape() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    assert!(
        ch.resolve_local_attachment_path("/workspace/../etc/passwd")
            .is_err(),
        "dotdot escape with /workspace/ prefix should be rejected"
    );
    assert!(
        ch.resolve_local_attachment_path("sub/../../etc/passwd")
            .is_err(),
        "relative dotdot escape should be rejected"
    );
}

#[test]
fn resolve_local_attachment_path_rejects_absolute_outside_workspace() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    assert!(
        ch.resolve_local_attachment_path("/etc/passwd").is_err(),
        "absolute path outside workspace should be rejected"
    );
    assert!(
        ch.resolve_local_attachment_path("file:///etc/passwd")
            .is_err(),
        "file URI outside workspace should be rejected"
    );
}

#[test]
#[cfg(unix)] // `std::os::unix::fs::symlink` is Unix-only; on Windows the
// lexical-only containment path is still exercised by the
// other tests in this module.
fn resolve_local_attachment_path_rejects_symlink_escaping_workspace() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let outside_dir = temp.path().join("outside-target");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside_file = outside_dir.join("secret.txt");
    std::fs::write(&outside_file, "top secret").unwrap();
    std::os::unix::fs::symlink(&outside_dir, workspace.join("outside")).unwrap();

    let ch = test_wechat_channel_with_workspace(&workspace);
    let err = ch
        .resolve_local_attachment_path("/workspace/outside/secret.txt")
        .expect_err("symlink that escapes workspace must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("canonicalizes to") && msg.contains("escapes workspace"),
        "expected canonical-escape error, got: {msg}"
    );
}

#[test]
#[cfg(unix)] // Symlink creation is Unix-only; the test still proves the
// canonical-containment path on the platforms where it runs.
fn resolve_local_attachment_path_accepts_symlink_within_workspace() {
    // Workspace-internal symlinks are legitimate aliases (e.g. a
    // `latest -> 2026-07-03` link inside an attachments directory).
    // They must still resolve cleanly so the upload sees the real file.
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let real_dir = workspace.join("attachments").join("2026-07-03");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real_file = real_dir.join("report.pdf");
    std::fs::write(&real_file, b"%PDF-1.4\n").unwrap();
    std::os::unix::fs::symlink(&real_dir, workspace.join("latest")).unwrap();

    let ch = test_wechat_channel_with_workspace(&workspace);
    let resolved = ch
        .resolve_local_attachment_path("/workspace/latest/report.pdf")
        .expect("workspace-internal symlink alias must be accepted");
    let real_canon = std::fs::canonicalize(&real_file).unwrap();
    assert_eq!(resolved, real_canon);
}

#[test]
fn resolve_local_attachment_path_allows_nonexistent_lexical_target() {
    // Non-existent paths must still pass (a future-write path, or a
    // target the agent has not created yet). The canonical-containment
    // check is skipped because canonicalize() would fail.
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    let resolved = ch
        .resolve_local_attachment_path("/workspace/not-yet-created.png")
        .expect("non-existent path under workspace is allowed (lexical only)");
    assert_eq!(resolved, workspace.join("not-yet-created.png"));
}

#[tokio::test]
async fn load_attachment_payload_rejects_path_traversal() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ch = test_wechat_channel_with_workspace(&workspace);
    let attachment = WeChatAttachment {
        kind: WeChatAttachmentKind::Image,
        target: "/workspace/../etc/passwd".to_string(),
    };
    let err = ch.load_attachment_payload(&attachment).await.unwrap_err();
    assert!(err.to_string().contains("escapes workspace"), "got: {err}");
}

#[test]
fn parse_aes_key_accepts_hex_and_base64() {
    let raw: [u8; 16] = *b"0123456789abcdef";
    let hex_key = hex::encode(raw);
    let base64_key = base64::engine::general_purpose::STANDARD.encode(raw);

    // Inbound accepts plain hex and base64(raw bytes).
    assert_eq!(parse_aes_key(&hex_key).unwrap(), raw);
    assert_eq!(parse_aes_key(&base64_key).unwrap(), raw);

    let outbound = base64::engine::general_purpose::STANDARD.encode(hex::encode(raw));
    assert_ne!(outbound, base64_key);
    assert_eq!(parse_aes_key(&outbound).unwrap(), raw);
}

#[test]
fn find_inbound_attachment_prefers_direct_media() {
    let items = vec![
        serde_json::json!({
            "type": 1,
            "text_item": { "text": "caption" },
            "ref_msg": {
                "message_item": {
                    "type": 4,
                    "file_item": {
                        "media": {
                            "encrypt_query_param": "quoted"
                        },
                        "file_name": "quoted.pdf"
                    }
                }
            }
        }),
        serde_json::json!({
            "type": 2,
            "image_item": {
                "media": {
                    "encrypt_query_param": "direct"
                }
            }
        }),
    ];

    let spec = WeChatChannel::find_inbound_attachment(&items, "123").unwrap();
    assert_eq!(spec.kind, WeChatAttachmentKind::Image);
    assert_eq!(spec.encrypted_query_param, "direct");
}

#[test]
fn markdown_to_plain_text_strips_common_formatting() {
    let input = "# Title\n**bold** [link](https://example.com)\n\n```rust\nlet x = 1;\n```";
    assert_eq!(
        markdown_to_plain_text(input),
        "Title\nbold link\n\nlet x = 1;"
    );
}

#[test]
fn build_base_info_includes_channel_version() {
    let base_info = build_base_info();
    let version = base_info
        .get("channel_version")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    assert!(!version.is_empty());
}

#[test]
fn sync_data_round_trip_preserves_context_tokens() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();

    let mut context_tokens = HashMap::new();
    context_tokens.insert("user123".to_string(), "token_abc".to_string());
    context_tokens.insert("user456".to_string(), "token_xyz".to_string());

    let original_data = SyncData {
        get_updates_buf: "cursor_value".to_string(),
        context_tokens: context_tokens.clone(),
    };

    let sync_path = state_dir.join("sync.json");
    let json = serde_json::to_string(&original_data).unwrap();
    write_private(&sync_path, json.as_bytes()).unwrap();

    let loaded_json = std::fs::read_to_string(&sync_path).unwrap();
    let loaded_data: SyncData = serde_json::from_str(&loaded_json).unwrap();

    assert_eq!(loaded_data.get_updates_buf, "cursor_value");
    assert_eq!(loaded_data.context_tokens.len(), 2);
    assert_eq!(
        loaded_data.context_tokens.get("user123"),
        Some(&"token_abc".to_string())
    );
    assert_eq!(
        loaded_data.context_tokens.get("user456"),
        Some(&"token_xyz".to_string())
    );
}

#[test]
fn sync_data_backward_compatible_with_missing_context_tokens() {
    let old_json = r#"{"get_updates_buf":"old_cursor"}"#;
    let data: SyncData = serde_json::from_str(old_json).unwrap();

    assert_eq!(data.get_updates_buf, "old_cursor");
    assert!(data.context_tokens.is_empty());
}

#[tokio::test]
async fn context_tokens_survive_channel_restart() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();

    {
        let ch = WeChatChannel::new(
            "test",
            Arc::new(|| vec!["*".to_string()]),
            None,
            None,
            Some(state_dir.clone()),
        )
        .unwrap();
        ch.set_context_token("acct1:userA", "tok_A").await.unwrap();
        ch.set_context_token("acct1:userB", "tok_B").await.unwrap();
        *ch.cursor.lock() = "cursor_123".to_string();
        ch.save_sync_data().unwrap();
    }

    let ch2 = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir),
    )
    .unwrap();

    assert_eq!(
        ch2.get_context_token("acct1:userA"),
        Some("tok_A".to_string())
    );
    assert_eq!(
        ch2.get_context_token("acct1:userB"),
        Some("tok_B".to_string())
    );
    assert_eq!(ch2.get_context_token("nonexistent"), None);
    assert_eq!(*ch2.cursor.lock(), "cursor_123");
}

#[tokio::test]
async fn set_context_token_persists_immediately() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();

    let ch = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir.clone()),
    )
    .unwrap();
    ch.set_context_token("acct:user1", "immediate_tok")
        .await
        .unwrap();

    let ch2 = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir),
    )
    .unwrap();
    assert_eq!(
        ch2.get_context_token("acct:user1"),
        Some("immediate_tok".to_string())
    );
}

#[tokio::test]
async fn save_sync_data_preserves_context_tokens() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();

    let ch = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir.clone()),
    )
    .unwrap();
    ch.set_context_token("acct:user1", "my_token")
        .await
        .unwrap();
    *ch.cursor.lock() = "new_cursor_value".to_string();
    ch.save_sync_data().unwrap();

    let ch2 = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir),
    )
    .unwrap();
    assert_eq!(*ch2.cursor.lock(), "new_cursor_value");
    assert_eq!(
        ch2.get_context_token("acct:user1"),
        Some("my_token".to_string())
    );
}

#[test]
fn load_from_empty_state_dir_produces_defaults() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();

    let ch = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir),
    )
    .unwrap();

    assert_eq!(ch.get_context_token("anything"), None);
    assert_eq!(*ch.cursor.lock(), "");
}

#[tokio::test]
async fn context_token_overwrite_persists_latest() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();

    let ch = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir.clone()),
    )
    .unwrap();
    ch.set_context_token("acct:user1", "old_token")
        .await
        .unwrap();
    ch.set_context_token("acct:user1", "new_token")
        .await
        .unwrap();

    let ch2 = WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir),
    )
    .unwrap();
    assert_eq!(
        ch2.get_context_token("acct:user1"),
        Some("new_token".to_string())
    );
}

#[test]
fn write_private_sets_owner_only_permissions() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("account.json");
    write_private(&path, b"{\"token\":\"x\"}").unwrap();
    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "{\"token\":\"x\"}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "durable file must be owner-read/write only");
    }
}

#[test]
fn write_private_failure_does_not_clobber_existing_file() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("sync.json");
    write_private(&path, b"{\"get_updates_buf\":\"old\"}").unwrap();

    // Make the directory unwritable so the temp-file create fails.
    // The existing durable file must remain intact.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp.path();
        let original = std::fs::metadata(dir).unwrap().permissions();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = write_private(&path, b"{\"get_updates_buf\":\"new\"}").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        std::fs::set_permissions(dir, original).unwrap();
    }
    #[cfg(not(unix))]
    {
        // On non-Unix, simulate failure by targeting a path whose parent
        // is a file rather than a directory.
        let blocker = temp.path().join("not-a-dir");
        std::fs::write(&blocker, b"file").unwrap();
        let nested = blocker.join("sync.json");
        assert!(write_private(&nested, b"new").is_err());
    }

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        contents, "{\"get_updates_buf\":\"old\"}",
        "a failed write must not truncate or replace the previous file"
    );
}

#[test]
fn write_private_replaces_existing_destination() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("sync.json");
    write_private(&path, b"{\"get_updates_buf\":\"old\"}").unwrap();
    write_private(&path, b"{\"get_updates_buf\":\"new\"}").unwrap();
    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        contents, "{\"get_updates_buf\":\"new\"}",
        "a successful write must replace an existing destination on every platform; Windows std::fs::rename is MoveFileExW+MOVEFILE_REPLACE_EXISTING"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

fn getupdates_batch(cursor: &str, msgs: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "ret": 0,
        "errcode": 0,
        "get_updates_buf": cursor,
        "msgs": msgs,
    })
}

fn load_persisted_wechat(state_dir: &std::path::Path) -> WeChatChannel {
    WeChatChannel::new(
        "test",
        Arc::new(|| vec!["*".to_string()]),
        None,
        None,
        Some(state_dir.to_path_buf()),
    )
    .unwrap()
}

fn test_wechat_channel_for_listen(
    mock_uri: String,
    state_dir: &Path,
    workspace_dir: &Path,
) -> WeChatChannel {
    let mut channel = test_wechat_channel_for_api(mock_uri.clone(), state_dir);
    channel.cdn_base_url = mock_uri;
    channel.workspace_dir = Some(workspace_dir.to_path_buf());
    channel
}

fn inbound_image_item() -> serde_json::Value {
    serde_json::json!({
        "type": 2,
        "image_item": {
            "media": {"encrypt_query_param": "enc_param_1"}
        }
    })
}

fn inbound_encrypted_image_item(aes_key: &str) -> serde_json::Value {
    serde_json::json!({
        "type": 2,
        "image_item": {
            "aeskey": aes_key,
            "media": {"encrypt_query_param": "enc_param_1"}
        }
    })
}

fn inbound_text_item(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": 1,
        "text_item": {"text": text}
    })
}

fn inbound_message(from: &str, message_id: u64, items: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "from_user_id": from,
        "message_id": message_id,
        "create_time_ms": 1_700_000_000_000u64,
        "item_list": items
    })
}

#[test]
fn cdn_status_classify_uses_permanent_whitelist() {
    let status400 = reqwest::StatusCode::BAD_REQUEST;
    let status403 = reqwest::StatusCode::FORBIDDEN;
    let status404 = reqwest::StatusCode::NOT_FOUND;
    let status408 = reqwest::StatusCode::REQUEST_TIMEOUT;
    let status410 = reqwest::StatusCode::GONE;
    let status425 = reqwest::StatusCode::from_u16(425).expect("425 is a valid status");
    let status429 = reqwest::StatusCode::TOO_MANY_REQUESTS;
    let status500 = reqwest::StatusCode::INTERNAL_SERVER_ERROR;

    assert_eq!(
        AttachmentBuildFailure::classify_status(status400, "gone").kind(),
        AttachmentFailureKind::Permanent
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status403, "forbidden").kind(),
        AttachmentFailureKind::Permanent
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status404, "missing").kind(),
        AttachmentFailureKind::Permanent
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status410, "gone").kind(),
        AttachmentFailureKind::Permanent
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status408, "timeout").kind(),
        AttachmentFailureKind::Transient,
        "408 must stay transient so a timed-out CDN object is retried"
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status425, "too early").kind(),
        AttachmentFailureKind::Transient,
        "a 4xx outside the permanent whitelist must stay transient"
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status429, "rate limited").kind(),
        AttachmentFailureKind::Transient
    );
    assert_eq!(
        AttachmentBuildFailure::classify_status(status500, "upstream").kind(),
        AttachmentFailureKind::Transient
    );
}

/// Regression for lost inbound batches: if the first `tx.send` in a batch
/// fails (receiver gone), `listen()` must return without committing the
/// cursor the response carried. Otherwise a crash between cursor
/// persistence and enqueue completion permanently skips the un-enqueued
/// messages on restart.
#[tokio::test]
async fn listen_does_not_commit_cursor_when_first_enqueue_fails() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([
                {
                    "from_user_id": "user_a",
                    "message_id": 1,
                    "create_time_ms": 1_700_000_000_000u64,
                    "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
                },
                {
                    "from_user_id": "user_b",
                    "message_id": 2,
                    "create_time_ms": 1_700_000_001_000u64,
                    "item_list": [{"type": 1, "text_item": {"text": "world"}}]
                }
            ]),
        )))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_api(mock_server.uri(), &state_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();

    // Drop the receiver before listen starts so the first send fails
    // without depending on scheduling between recv and drop.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);

    let result = tokio::time::timeout(Duration::from_secs(5), ch.listen(tx))
        .await
        .expect("listen() should return promptly once the receiver is gone");
    assert!(result.is_ok());

    let probe = load_persisted_wechat(&state_dir);
    assert_eq!(
        *probe.cursor.lock(),
        "original_cursor",
        "cursor must not advance when the batch was never enqueued"
    );
}

/// Happy path for the deferred cursor commit: once a batch is fully
/// enqueued, its cursor commits. A second batch whose enqueue fails
/// must not move the cursor further.
///
/// Determinism: capacity 2 holds batch 1 entirely. Batch 2's send blocks
/// until we drop the receiver, so the test can observe `cursor_batch_1`
/// on disk before the next send is allowed to complete.
#[tokio::test]
async fn listen_commits_cursor_only_after_batch_fully_enqueued() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_batch_1",
            serde_json::json!([
                {
                    "from_user_id": "user_a",
                    "message_id": 1,
                    "create_time_ms": 1_700_000_000_000u64,
                    "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
                },
                {
                    "from_user_id": "user_b",
                    "message_id": 2,
                    "create_time_ms": 1_700_000_001_000u64,
                    "item_list": [{"type": 1, "text_item": {"text": "world"}}]
                }
            ]),
        )))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_batch_2",
            serde_json::json!([
                {
                    "from_user_id": "user_c",
                    "message_id": 3,
                    "create_time_ms": 1_700_000_002_000u64,
                    "item_list": [{"type": 1, "text_item": {"text": "third"}}]
                }
            ]),
        )))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_api(mock_server.uri(), &state_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, rx) = tokio::sync::mpsc::channel(2);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let probe = load_persisted_wechat(&state_dir);
        if *probe.cursor.lock() == "cursor_batch_1" && rx.len() == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for batch 1 to enqueue and persist; cursor={} queued={}",
            *probe.cursor.lock(),
            rx.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        *load_persisted_wechat(&state_dir).cursor.lock(),
        "cursor_batch_1",
        "mid-wait: only batch 1's cursor may be on disk (batch 2 send is blocked)"
    );

    drop(rx);

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("listen() task timed out")
        .expect("listen() task panicked");
    assert!(result.is_ok());

    let probe = load_persisted_wechat(&state_dir);
    assert_eq!(
        *probe.cursor.lock(),
        "cursor_batch_1",
        "cursor should advance to batch 1's cursor, not batch 2's"
    );
}

/// `set_context_token` (called mid-batch) itself persists sync data.
/// Because cursor commitment is deferred until the whole batch is
/// enqueued, that mid-batch save must persist the OLD cursor while still
/// recording the new context token.
///
/// Determinism: capacity 1 lets the first send complete and blocks the
/// second. The test asserts disk state while the second send is blocked,
/// then drops the receiver so listen exits without committing.
#[tokio::test]
async fn listen_mid_batch_context_token_save_keeps_old_cursor() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().to_path_buf();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([
                {
                    "from_user_id": "user_a",
                    "message_id": 1,
                    "create_time_ms": 1_700_000_000_000u64,
                    "context_token": "ctx_abc123",
                    "item_list": [{"type": 1, "text_item": {"text": "hello"}}]
                },
                {
                    "from_user_id": "user_b",
                    "message_id": 2,
                    "create_time_ms": 1_700_000_001_000u64,
                    "item_list": [{"type": 1, "text_item": {"text": "world"}}]
                }
            ]),
        )))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_api(mock_server.uri(), &state_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let probe = load_persisted_wechat(&state_dir);
        let token_ready = probe.get_context_token("user_a").as_deref() == Some("ctx_abc123");
        if token_ready && !rx.is_empty() {
            assert_eq!(
                *probe.cursor.lock(),
                "original_cursor",
                "mid-batch save must not have leaked the uncommitted new cursor"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for mid-batch token persist with first message queued"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(rx);

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("listen() task timed out")
        .expect("listen() task panicked");
    assert!(result.is_ok());

    let probe = load_persisted_wechat(&state_dir);
    assert_eq!(
        probe.get_context_token("user_a"),
        Some("ctx_abc123".to_string()),
        "mid-batch set_context_token must still persist the new token"
    );
    assert_eq!(
        *probe.cursor.lock(),
        "original_cursor",
        "mid-batch save must not have leaked the uncommitted new cursor"
    );
}

/// A pure-attachment message whose CDN download fails transiently (503)
/// must not advance the cursor. Folding that failure to `None` and
/// committing the batch would permanently skip a message that has no
/// text fallback. Once the CDN recovers, the same listener re-fetches
/// the held batch, delivers the attachment, and then commits.
#[tokio::test]
async fn listen_holds_cursor_on_transient_pure_attachment_failure_then_advances() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let workspace_dir = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let mock_server = MockServer::start().await;

    let held_batch = getupdates_batch(
        "cursor_after_batch",
        serde_json::json!([inbound_message(
            "user_a",
            1,
            serde_json::json!([inbound_image_item()])
        )]),
    );

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .and(body_partial_json(
            serde_json::json!({"get_updates_buf": "original_cursor"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(held_batch))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .and(body_partial_json(
            serde_json::json!({"get_updates_buf": "cursor_after_batch"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([]),
        )))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-image-bytes".to_vec()))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_listen(mock_server.uri(), &state_dir, &workspace_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    let probe = load_persisted_wechat(&state_dir);
    assert_eq!(
        *probe.cursor.lock(),
        "original_cursor",
        "a transient CDN failure must not commit the batch cursor"
    );
    assert!(
        rx.try_recv().is_err(),
        "a held batch must not deliver a degraded or empty stand-in"
    );

    let delivered = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for the attachment after CDN recovery")
        .expect("channel closed before the recovered attachment arrived");
    assert_eq!(delivered.sender, "user_a");
    assert!(
        delivered.content.contains("[IMAGE:"),
        "pure-attachment must be delivered once the CDN recovers, got: {}",
        delivered.content
    );

    handle.abort();
    let _ = handle.await;

    let probe = load_persisted_wechat(&state_dir);
    assert_eq!(
        *probe.cursor.lock(),
        "cursor_after_batch",
        "cursor must advance once the recovered attachment is enqueued"
    );
}

/// A deterministic permanent CDN rejection (404) on a pure-attachment
/// message is a logged skip, not a hold. The batch must still commit so
/// the listener cannot wedge on an object that will never appear.
#[tokio::test]
async fn listen_advances_cursor_when_pure_attachment_fails_permanently() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let workspace_dir = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([inbound_message(
                "user_a",
                1,
                serde_json::json!([inbound_image_item()])
            )]),
        )))
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_listen(mock_server.uri(), &state_dir, &workspace_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let probe = load_persisted_wechat(&state_dir);
        if *probe.cursor.lock() == "cursor_after_batch" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a permanent attachment skip to commit the cursor"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        rx.try_recv().is_err(),
        "a permanently skipped pure-attachment must not enqueue a stand-in"
    );

    handle.abort();
    let _ = handle.await;
}

/// Permanent attachment failure on a mixed text+image message still
/// delivers the text and advances the cursor. The attachment is
/// omitted rather than holding the whole batch.
#[tokio::test]
async fn listen_delivers_text_and_advances_when_attachment_fails_permanently() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let workspace_dir = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([inbound_message(
                "user_a",
                1,
                serde_json::json!([inbound_text_item("hello"), inbound_image_item()])
            )]),
        )))
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_listen(mock_server.uri(), &state_dir, &workspace_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    let delivered = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for the text after a permanent attachment skip")
        .expect("channel closed before the text arrived");
    assert_eq!(delivered.sender, "user_a");
    assert!(
        delivered.content.contains("[Image: unavailable]"),
        "a permanent attachment skip must annotate the kept text, got: {}",
        delivered.content
    );
    assert!(
        delivered.content.contains("hello"),
        "a permanent attachment skip must still deliver the text, got: {}",
        delivered.content
    );
    assert!(
        !delivered.content.contains("[IMAGE:"),
        "must not fabricate a successful image payload marker, got: {}",
        delivered.content
    );

    handle.abort();
    let _ = handle.await;

    let probe = load_persisted_wechat(&state_dir);
    assert_eq!(
        *probe.cursor.lock(),
        "cursor_after_batch",
        "a permanent attachment skip must still commit the batch cursor"
    );
}

/// Combining typed disposition with cursor-after-enqueue: a text
/// message followed by a retryable attachment must not publish the
/// earlier text while the batch is held. Publishing it would
/// redeliver that text on every retry pass.
#[tokio::test]
async fn listen_does_not_publish_prior_text_while_later_attachment_is_held() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let temp = tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let workspace_dir = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let mock_server = MockServer::start().await;

    let held_batch = getupdates_batch(
        "cursor_after_batch",
        serde_json::json!([
            inbound_message("user_a", 1, serde_json::json!([inbound_text_item("first")])),
            inbound_message("user_b", 2, serde_json::json!([inbound_image_item()])),
        ]),
    );

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .and(body_partial_json(
            serde_json::json!({"get_updates_buf": "original_cursor"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(held_batch))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .and(body_partial_json(
            serde_json::json!({"get_updates_buf": "cursor_after_batch"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([]),
        )))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-image-bytes".to_vec()))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_listen(mock_server.uri(), &state_dir, &workspace_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        rx.try_recv().is_err(),
        "text ahead of a retryable attachment must stay staged until the whole batch resolves"
    );
    assert_eq!(
        *load_persisted_wechat(&state_dir).cursor.lock(),
        "original_cursor"
    );

    let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for staged text after CDN recovery")
        .expect("channel closed before staged text arrived");
    let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for recovered attachment")
        .expect("channel closed before recovered attachment arrived");

    assert_eq!(first.sender, "user_a");
    assert_eq!(first.content, "first");
    assert_eq!(second.sender, "user_b");
    assert!(
        second.content.contains("[IMAGE:"),
        "attachment must arrive after the staged text, got: {}",
        second.content
    );
    assert!(
        rx.try_recv().is_err(),
        "the recovered batch must deliver each message once"
    );

    handle.abort();
    let _ = handle.await;

    assert_eq!(
        *load_persisted_wechat(&state_dir).cursor.lock(),
        "cursor_after_batch"
    );
}

/// A 200 CDN body that fails PKCS7 decrypt (truncated/corrupt ciphertext)
/// must be transient: the key metadata was valid, so a later complete
/// download can recover. Treating decrypt failure as permanent would
/// skip the message and commit the cursor — the same silent loss this
/// change exists to close.
#[tokio::test]
async fn listen_retries_truncated_encrypted_attachment_then_advances() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let aes_hex = "0123456789abcdef0123456789abcdef";
    let key = parse_aes_key(aes_hex).expect("test AES key must parse");
    let ciphertext = encrypt_aes_ecb(b"fake-image-bytes", &key).expect("encrypt fixture");

    let temp = tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let workspace_dir = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let mock_server = MockServer::start().await;

    let held_batch = getupdates_batch(
        "cursor_after_batch",
        serde_json::json!([inbound_message(
            "user_a",
            1,
            serde_json::json!([inbound_encrypted_image_item(aes_hex)])
        )]),
    );

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .and(body_partial_json(
            serde_json::json!({"get_updates_buf": "original_cursor"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(held_batch))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .and(body_partial_json(
            serde_json::json!({"get_updates_buf": "cursor_after_batch"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(getupdates_batch(
            "cursor_after_batch",
            serde_json::json!([]),
        )))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"truncated".to_vec()))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(ciphertext))
        .mount(&mock_server)
        .await;

    let ch = test_wechat_channel_for_listen(mock_server.uri(), &state_dir, &workspace_dir);
    *ch.cursor.lock() = "original_cursor".to_string();
    ch.save_sync_data().unwrap();
    let ch = Arc::new(ch);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listen_ch = ch.clone();
    let handle = zeroclaw_spawn::spawn!(async move { listen_ch.listen(tx).await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        *load_persisted_wechat(&state_dir).cursor.lock(),
        "original_cursor",
        "a truncated encrypted payload must not commit the batch cursor"
    );
    assert!(
        rx.try_recv().is_err(),
        "a truncated decrypt must hold the batch, not skip the attachment"
    );

    let delivered = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for the attachment after a complete ciphertext retry")
        .expect("channel closed before the recovered attachment arrived");
    assert_eq!(delivered.sender, "user_a");
    assert!(
        delivered.content.contains("[IMAGE:"),
        "recovered ciphertext must deliver the attachment, got: {}",
        delivered.content
    );

    handle.abort();
    let _ = handle.await;

    assert_eq!(
        *load_persisted_wechat(&state_dir).cursor.lock(),
        "cursor_after_batch",
        "cursor must advance once the recovered ciphertext is enqueued"
    );
}
