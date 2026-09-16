#[cfg(test)]
use super::*;

#[test]
fn scope_uses_group_shared_mode_by_default_for_group_chat() {
    let inbound = ParsedInbound {
        msg_id: "m1".to_string(),
        msg_type: "text".to_string(),
        chat_type: "group".to_string(),
        chat_id: Some("g1".to_string()),
        sender_userid: "u1".to_string(),
        aibot_id: "b1".to_string(),
        raw_payload: serde_json::json!({}),
    };

    let scopes = compute_scopes(&inbound);
    assert_eq!(scopes.reply_target, "group--g1");
    assert_eq!(
        scopes.conversation_scope,
        ChannelConversationScope::ReplyTarget
    );
    assert_eq!(scopes.interruption_scope_id.as_deref(), Some("group--g1"));
}

#[test]
fn scope_uses_reply_target_for_single_chat_without_thread_fragmentation() {
    let inbound = test_inbound("single", None, "user-1");
    let scopes = compute_scopes(&inbound);

    assert_eq!(scopes.reply_target, "user--user-1");
    assert_eq!(
        scopes.conversation_scope,
        ChannelConversationScope::ReplyTarget
    );
    assert!(scopes.interruption_scope_id.is_none());

    let first = scopes.channel_message(
        "work",
        "msg-1",
        "user-1",
        "hello",
        Some("req-1".to_string()),
        false,
    );
    let second = scopes.channel_message(
        "work",
        "msg-2",
        "user-1",
        "follow up",
        Some("req-2".to_string()),
        false,
    );
    let clear = scopes.channel_message(
        "work",
        "msg-3",
        "user-1",
        "/new",
        Some("req-clear".to_string()),
        false,
    );

    let first_key = crate::orchestrator::conversation_history_key(&first);
    assert_eq!(
        first_key,
        crate::orchestrator::conversation_history_key(&second)
    );
    assert_eq!(
        first_key,
        crate::orchestrator::conversation_history_key(&clear)
    );
    assert!(!first_key.contains("req-"));
}

#[test]
fn split_markdown_chunks_preserves_large_input() {
    let input = "a".repeat(WECOM_MARKDOWN_CHUNK_BYTES * 3 + 100);
    let chunks = split_markdown_chunks(&input);
    assert!(chunks.len() >= 3);
    for chunk in chunks {
        assert!(chunk.len() <= WECOM_MARKDOWN_MAX_BYTES);
    }
}

#[test]
fn split_markdown_chunks_small_input() {
    let input = "Hello WeCom!";
    let chunks = split_markdown_chunks(input);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], "Hello WeCom!");
}

#[test]
fn split_markdown_chunks_empty_input() {
    let chunks = split_markdown_chunks("");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], "");
}

#[test]
fn strip_trailing_provider_sentinels_removes_eom_token() {
    assert_eq!(
        strip_trailing_provider_sentinels("Hi there!<|eom|>"),
        "Hi there!"
    );
    assert_eq!(
        strip_trailing_provider_sentinels("Hi there!  <|eom|>\n\n"),
        "Hi there!"
    );
}

#[test]
fn strip_trailing_provider_sentinels_keeps_mid_message_token() {
    assert_eq!(
        strip_trailing_provider_sentinels("Literal <|eom|> marker in text."),
        "Literal <|eom|> marker in text."
    );
}

#[test]
fn outbound_stream_normalization_strips_trailing_provider_sentinel() {
    assert_eq!(normalize_stream_content("Hi there!<|eom|>"), "Hi there!");
    assert_eq!(
        split_stream_content_and_overflow("Hi there!<|eom|>"),
        ("Hi there!".to_string(), None)
    );
    assert_eq!(split_markdown_chunks("Hi there!<|eom|>"), vec!["Hi there!"]);
}

#[test]
fn group_bot_mention_sets_structured_addressing_without_content_marker() {
    let inbound = test_inbound("group", Some("group-1"), "user-1");
    let composed = compose_content_for_framework(&inbound, "@danya say hi");

    assert_eq!(composed, "@danya say hi");
    assert!(message_explicitly_addresses_bot(
        &inbound,
        "@danya say hi",
        Some("danya")
    ));
}

#[test]
fn group_bot_addressing_omits_non_matching_messages() {
    let inbound = test_inbound("group", Some("group-1"), "user-1");
    assert_eq!(
        compose_content_for_framework(&inbound, "@otherbot say hi"),
        "@otherbot say hi"
    );
    assert!(!message_explicitly_addresses_bot(
        &inbound,
        "@otherbot say hi",
        Some("danya")
    ));
    assert!(!message_explicitly_addresses_bot(
        &inbound,
        "@danya say hi",
        None
    ));

    let dm = test_inbound("single", None, "user-1");
    assert_eq!(
        compose_content_for_framework(&dm, "@danya say hi"),
        "@danya say hi"
    );
    assert!(!message_explicitly_addresses_bot(
        &dm,
        "@danya say hi",
        Some("danya")
    ));
}

#[test]
fn text_mentions_bot_name_uses_simple_boundary_check() {
    assert!(text_mentions_bot_name("@danya say hi", "danya"));
    assert!(text_mentions_bot_name("hey @danya, say hi", "danya"));
    assert!(text_mentions_bot_name("@danya，帮我看一下", "danya"));
    assert!(text_mentions_bot_name("@danya：帮我看一下", "danya"));
    assert!(!text_mentions_bot_name("@danyabot say hi", "danya"));
}

#[test]
fn summarize_attachment_url_for_log_redacts_query_string() {
    let url = "https://wework.qpic.cn/wwpic/123456/0?auth=secret_token&expires=123";
    let summary = summarize_attachment_url_for_log(url);
    assert_eq!(
        summary,
        "https://wework.qpic.cn/wwpic/123456/0 (query=present)"
    );
    assert!(!summary.contains("secret_token"));
}

#[test]
fn summarize_attachment_url_for_log_handles_invalid_input() {
    let summary = summarize_attachment_url_for_log("not a url");
    assert_eq!(summary, "invalid-url(len=9)");
}

#[test]
fn stop_command_detection_supports_cn_and_en() {
    assert!(contains_stop_command("\u{505c}\u{6b62}"));
    assert!(contains_stop_command("Please STOP now"));
    assert!(contains_stop_command("@bot /stop"));
    assert!(!contains_stop_command("\u{7ee7}\u{7eed}\u{5904}\u{7406}"));
    assert!(!contains_stop_command("explain nonstop operation"));
    assert!(!contains_stop_command("what are stopwords?"));
}

#[test]
fn image_file_extension_uses_magic_bytes() {
    assert_eq!(image_file_extension(b"\x89PNG\r\n\x1a\nrest"), "png");
    assert_eq!(image_file_extension(&[0xff, 0xd8, 0xff, 0x00]), "jpg");
    assert_eq!(image_file_extension(b"GIF89a rest"), "gif");
    assert_eq!(
        image_file_extension(b"RIFF\x00\x00\x00\x00WEBPrest"),
        "webp"
    );
    assert_eq!(image_file_extension(b"not an image"), "bin");
}

#[test]
fn filename_scope_components_reject_path_separators() {
    assert_eq!(normalize_scope_component("../room/msg-1"), "___room_msg-1");
}

#[test]
fn idempotency_store_is_bounded() {
    let store = SimpleIdempotencyStore::new();
    for idx in 0..(WECOM_IDEMPOTENCY_MAX_KEYS + 1) {
        assert!(store.record_if_new(&format!("msg-{idx}")));
    }
    assert_eq!(store.seen.lock().len(), WECOM_IDEMPOTENCY_MAX_KEYS);
    assert_eq!(store.order.lock().len(), WECOM_IDEMPOTENCY_MAX_KEYS);
    assert!(store.record_if_new("msg-0"));
}

#[test]
fn parse_event_type_extracts_enter_chat() {
    let payload = serde_json::json!({
        "event": {
            "eventtype": "enter_chat"
        }
    });
    assert_eq!(parse_event_type(&payload).as_deref(), Some("enter_chat"));
}

#[test]
fn extract_quote_context_from_text_quote() {
    let payload = serde_json::json!({
        "quote": {
            "msgtype": "text",
            "text": {
                "content": "  \u{5f15}\u{7528}\u{5185}\u{5bb9}  "
            }
        }
    });

    let quote = extract_quote_context(&payload).expect("quote should be extracted");
    assert!(quote.contains("msgtype=text"));
    assert!(quote.contains("content=\u{5f15}\u{7528}\u{5185}\u{5bb9}"));
}

#[test]
fn extract_quote_context_from_mixed_quote() {
    let payload = serde_json::json!({
        "quote": {
            "msgtype": "mixed",
            "mixed": {
                "msg_item": [
                    {
                        "msgtype": "text",
                        "text": {
                            "content": "\u{7b2c}\u{4e00}\u{6bb5}"
                        }
                    },
                    {
                        "msgtype": "image",
                        "image": {
                            "url": "https://example.com/image.png"
                        }
                    }
                ]
            }
        }
    });

    let quote = extract_quote_context(&payload).expect("quote should be extracted");
    assert!(quote.contains("\u{7b2c}\u{4e00}\u{6bb5}"));
    assert!(quote.contains("\u{5f15}\u{7528}\u{56fe}\u{7247}"));
}

#[test]
fn extract_quote_context_does_not_leak_remote_media_url() {
    let payload = serde_json::json!({
        "quote": {
            "msgtype": "image",
            "image": {
                "url": "https://example.com/tmp-sign-url"
            }
        }
    });

    let quote = extract_quote_context(&payload).expect("quote should be extracted");
    assert!(quote.contains("[\u{5f15}\u{7528}\u{56fe}\u{7247}]"));
    assert!(!quote.contains("example.com/tmp-sign-url"));
}

#[test]
fn extract_template_card_event_key_reads_event_key() {
    let payload = serde_json::json!({
        "event": {
            "eventtype": "template_card_event",
            "template_card_event": {
                "event_key": "button_confirm"
            }
        }
    });
    assert_eq!(
        extract_template_card_event_key(&payload).as_deref(),
        Some("button_confirm")
    );
}

#[test]
fn extract_feedback_event_summary_reads_fields() {
    let payload = serde_json::json!({
        "event": {
            "eventtype": "feedback_event",
            "feedback_event": {
                "id": "fb_1",
                "type": 2,
                "content": "not accurate"
            }
        }
    });
    let summary = extract_feedback_event_summary(&payload).expect("summary should exist");
    assert!(summary.contains("feedback_id=fb_1"));
    assert!(summary.contains("feedback_type=2"));
    assert!(summary.contains("content=not accurate"));
}

#[test]
fn clear_session_bare_commands() {
    assert!(is_clear_session_command("/clear"));
    assert!(is_clear_session_command("/new"));
    assert!(is_clear_session_command("/CLEAR"));
    assert!(is_clear_session_command("/New"));
    assert!(is_clear_session_command("  /clear  "));
}

#[test]
fn clear_session_with_mentions() {
    assert!(is_clear_session_command("@bot /clear"));
    assert!(is_clear_session_command("/clear @bot"));
    assert!(is_clear_session_command("@bot1 @bot2 /new"));
    assert!(is_clear_session_command("@bot /new @other"));
}

#[test]
fn clear_session_rejects_old_and_invalid() {
    assert!(!is_clear_session_command("\u{65b0}\u{4f1a}\u{8bdd}"));
    assert!(!is_clear_session_command("clear history"));
    assert!(!is_clear_session_command("/clear now"));
    assert!(!is_clear_session_command("please /new"));
    assert!(!is_clear_session_command(""));
    assert!(!is_clear_session_command("   "));
}

#[test]
fn runtime_model_switch_command_with_mentions() {
    assert_eq!(
        extract_runtime_model_switch_command("@bot /model gpt-5 @other"),
        Some("/model gpt-5".to_string())
    );
    assert_eq!(
        extract_runtime_model_switch_command("@bot /models openrouter"),
        Some("/models openrouter".to_string())
    );
    assert_eq!(
        extract_runtime_model_switch_command(" /MODEL@zeroclaw qwen-max "),
        Some("/MODEL@zeroclaw qwen-max".to_string())
    );
}

#[test]
fn runtime_model_switch_command_rejects_non_commands() {
    assert_eq!(extract_runtime_model_switch_command("/new"), None);
    assert_eq!(
        extract_runtime_model_switch_command("please /model gpt-5"),
        None
    );
    assert_eq!(extract_runtime_model_switch_command(""), None);
}

#[test]
fn parse_scope_user() {
    let (chat_type, chatid) = parse_scope("user--zeroclaw_user").unwrap();
    assert_eq!(chat_type, 1);
    assert_eq!(chatid, "zeroclaw_user");
}

#[test]
fn parse_scope_group() {
    let (chat_type, chatid) = parse_scope("group--zeroclaw_group").unwrap();
    assert_eq!(chat_type, 2);
    assert_eq!(chatid, "zeroclaw_group");
}

#[test]
fn parse_scope_invalid() {
    assert!(parse_scope("invalid_scope").is_err());
}

fn test_inbound(chat_type: &str, chat_id: Option<&str>, sender_userid: &str) -> ParsedInbound {
    ParsedInbound {
        msg_id: "msg-1".to_string(),
        msg_type: "text".to_string(),
        chat_type: chat_type.to_string(),
        chat_id: chat_id.map(str::to_string),
        sender_userid: sender_userid.to_string(),
        aibot_id: "bot123".to_string(),
        raw_payload: serde_json::json!({
            "msgtype": "text",
            "msgid": "msg-1",
            "chattype": chat_type,
            "chatid": chat_id,
            "from": { "userid": sender_userid },
            "text": { "content": "@bot hello" }
        }),
    }
}

fn test_wecom_ws_config() -> WeComWsConfig {
    WeComWsConfig {
        enabled: true,
        bot_id: "bot123".to_string(),
        secret: "secret456".to_string(),
        allowed_users: vec![],
        allowed_groups: vec![],
        bot_name: None,
        file_retention_days: 3,
        max_file_size_mb: 20,
        stream_mode: StreamMode::Partial,
        proxy_url: None,
        excluded_tools: vec![],
    }
}

#[test]
fn runtime_policy_normalizes_config_and_external_peers() {
    let mut config = test_wecom_ws_config();
    config.allowed_users = vec![" user-1 ".to_string(), "".to_string()];
    config.allowed_groups = vec![" group-1 ".to_string()];
    config.bot_name = Some(" danya ".to_string());

    let policy = WeComWsRuntimePolicy::from_config(
        &config,
        vec![
            "user-1".to_string(),
            " external-1 ".to_string(),
            "".to_string(),
        ],
    );

    assert_eq!(
        policy.direct_userids,
        vec!["user-1".to_string(), "external-1".to_string()]
    );
    assert_eq!(policy.allowed_groups, vec!["group-1".to_string()]);
    assert_eq!(policy.bot_name.as_deref(), Some("danya"));
}

#[test]
fn channel_access_uses_live_runtime_policy() {
    let mut config = test_wecom_ws_config();
    config.allowed_users = vec!["user-1".to_string()];
    let policy = Arc::new(Mutex::new(WeComWsRuntimePolicy::from_config(
        &config,
        std::iter::empty(),
    )));
    let policy_resolver = {
        let policy = policy.clone();
        Arc::new(move || policy.lock().clone())
    };
    let channel =
        WeComWsChannel::new_with_alias(&config, "primary", policy_resolver, Path::new("/tmp"))
            .unwrap();
    let inbound = test_inbound("group", Some("group-1"), "blocked-user");

    assert_eq!(channel.access_decision(&inbound), AccessDecision::Denied);

    policy.lock().allowed_groups = vec!["group-1".to_string()];

    assert_eq!(channel.access_decision(&inbound), AccessDecision::Allowed);
}

#[test]
fn channel_bot_addressing_uses_live_runtime_policy() {
    let mut config = test_wecom_ws_config();
    config.bot_name = Some("danya".to_string());
    let policy = Arc::new(Mutex::new(WeComWsRuntimePolicy::from_config(
        &config,
        std::iter::empty(),
    )));
    let policy_resolver = {
        let policy = policy.clone();
        Arc::new(move || policy.lock().clone())
    };
    let channel =
        WeComWsChannel::new_with_alias(&config, "primary", policy_resolver, Path::new("/tmp"))
            .unwrap();
    let inbound = test_inbound("group", Some("group-1"), "user-1");

    assert!(channel.message_explicitly_addresses_bot(&inbound, "@danya say hi"));

    policy.lock().bot_name = Some("otherbot".to_string());

    assert!(!channel.message_explicitly_addresses_bot(&inbound, "@danya say hi"));
}

#[test]
fn access_decision_denies_when_allowlists_missing() {
    let inbound = test_inbound("single", None, "zeroclaw_user");
    assert_eq!(
        evaluate_access_decision(&[], &[], &inbound),
        AccessDecision::AllowlistMissing
    );
}

#[test]
fn access_decision_allows_userid_in_single_chat() {
    let inbound = test_inbound("single", None, "zeroclaw_user");
    assert_eq!(
        evaluate_access_decision(&["zeroclaw_user".to_string()], &[], &inbound),
        AccessDecision::Allowed
    );
}

#[test]
fn access_decision_allows_group_chatid() {
    let inbound = test_inbound("group", Some("zeroclaw_group"), "zeroclaw_user");
    assert_eq!(
        evaluate_access_decision(&[], &["zeroclaw_group".to_string()], &inbound),
        AccessDecision::Allowed
    );
}

#[test]
fn access_decision_allows_wildcards() {
    let inbound = test_inbound("group", Some("zeroclaw_group"), "zeroclaw_user");
    assert_eq!(
        evaluate_access_decision(&["*".to_string()], &[], &inbound),
        AccessDecision::Allowed
    );
    assert_eq!(
        evaluate_access_decision(&[], &["*".to_string()], &inbound),
        AccessDecision::Allowed
    );
}

#[test]
fn denied_group_message_mentions_chatid_and_userid() {
    let inbound = test_inbound("group", Some("zeroclaw_group"), "zeroclaw_user");
    let text = build_access_denied_message(&inbound, AccessDecision::Denied, "primary");
    assert!(text.contains("zeroclaw_group"));
    assert!(text.contains("zeroclaw_user"));
    assert!(text.contains("allowed_groups"));
    assert!(text.contains("wecom_ws"));
}

#[test]
fn supports_draft_updates_respects_stream_mode() {
    let mut off_cfg = test_wecom_ws_config();
    off_cfg.stream_mode = StreamMode::Off;
    let off = WeComWsChannel::new(&off_cfg, Path::new("/tmp")).unwrap();
    assert!(!off.supports_draft_updates());

    let partial = WeComWsChannel::new(&test_wecom_ws_config(), Path::new("/tmp")).unwrap();
    assert!(partial.supports_draft_updates());
}

#[test]
fn single_chat_reply_target_is_direct_message() {
    let channel = WeComWsChannel::new(&test_wecom_ws_config(), Path::new("/tmp")).unwrap();
    let single = ChannelMessage::new("msg-1", "user-1", "user--user-1", "hi", "wecom_ws", 1);
    let group = ChannelMessage::new("msg-2", "user-1", "group--group-1", "hi", "wecom_ws", 1);

    assert!(channel.is_direct_message(&single));
    assert!(!channel.is_direct_message(&group));
}

#[test]
fn multi_message_stream_mode_is_rejected() {
    let mut cfg = test_wecom_ws_config();
    cfg.stream_mode = StreamMode::MultiMessage;
    let err = match WeComWsChannel::new(&cfg, Path::new("/tmp")) {
        Ok(_) => panic!("multi_message should be rejected"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("multi_message is not supported"));
}

#[tokio::test]
async fn send_draft_returns_none_when_stream_mode_off() {
    let mut cfg = test_wecom_ws_config();
    cfg.stream_mode = StreamMode::Off;
    let channel = WeComWsChannel::new(&cfg, Path::new("/tmp")).unwrap();

    let id = channel
        .send_draft(&SendMessage::new("draft", "user--zeroclaw_user"))
        .await
        .unwrap();

    assert!(id.is_none());
}

#[tokio::test]
async fn send_draft_failure_does_not_record_req_id_mapping() {
    let channel = WeComWsChannel::new(&test_wecom_ws_config(), Path::new("/tmp")).unwrap();
    let result = channel
        .send_draft(
            &SendMessage::new("draft", "user--zeroclaw_user")
                .in_thread(Some("req-draft".to_string())),
        )
        .await;

    assert!(result.is_err());
    assert!(channel.req_id_map.lock().is_empty());
}

#[tokio::test]
async fn finalize_draft_failure_cleans_req_id_mapping() {
    let channel = WeComWsChannel::new(&test_wecom_ws_config(), Path::new("/tmp")).unwrap();
    channel
        .req_id_map
        .lock()
        .insert("stream-1".to_string(), "req-finalize".to_string());

    let result = channel
        .finalize_draft("user--zeroclaw_user", "stream-1", "final", false)
        .await;

    assert!(result.is_err());
    assert!(channel.req_id_map.lock().is_empty());
}

#[tokio::test]
async fn send_with_req_id_uses_respond_msg_when_stream_mode_off() {
    let mut cfg = test_wecom_ws_config();
    cfg.stream_mode = StreamMode::Off;
    let channel = WeComWsChannel::new(&cfg, Path::new("/tmp")).unwrap();

    let (ws_tx, mut ws_rx) = mpsc::channel::<WsOutbound>(4);
    *channel.ws_tx.lock().await = Some(ws_tx);

    let responder_channel = channel.clone();
    let responder = zeroclaw_spawn::spawn!(async move {
        let Some(WsOutbound::Frame(frame)) = ws_rx.recv().await else {
            panic!("expected respond_msg frame");
        };
        let req_id = frame
            .get("headers")
            .and_then(|headers| headers.get("req_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        responder_channel
            .maybe_handle_command_response(&serde_json::json!({
                "headers": { "req_id": req_id },
                "errcode": 0,
                "errmsg": "ok"
            }))
            .await;
        frame
    });

    channel
        .send(
            &SendMessage::new("runtime ok", "user--zeroclaw_user")
                .in_thread(Some("req-runtime".to_string())),
        )
        .await
        .unwrap();

    let frame = responder.await.unwrap();
    assert_eq!(
        frame.get("cmd").and_then(Value::as_str),
        Some("aibot_respond_msg")
    );
    assert_eq!(
        frame
            .get("headers")
            .and_then(|headers| headers.get("req_id"))
            .and_then(Value::as_str),
        Some("req-runtime")
    );
    assert_eq!(
        frame
            .pointer("/body/stream/content")
            .and_then(Value::as_str),
        Some("runtime ok")
    );
    assert_eq!(
        frame
            .pointer("/body/stream/finish")
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
async fn send_without_req_id_uses_send_msg() {
    let channel = WeComWsChannel::new(&test_wecom_ws_config(), Path::new("/tmp")).unwrap();

    let (ws_tx, mut ws_rx) = mpsc::channel::<WsOutbound>(4);
    *channel.ws_tx.lock().await = Some(ws_tx);

    let responder_channel = channel.clone();
    let responder = zeroclaw_spawn::spawn!(async move {
        let Some(WsOutbound::Frame(frame)) = ws_rx.recv().await else {
            panic!("expected send_msg frame");
        };
        let req_id = frame
            .get("headers")
            .and_then(|headers| headers.get("req_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        responder_channel
            .maybe_handle_command_response(&serde_json::json!({
                "headers": { "req_id": req_id },
                "errcode": 0,
                "errmsg": "ok"
            }))
            .await;
        frame
    });

    channel
        .send(&SendMessage::new("hello proactive", "user--zeroclaw_user"))
        .await
        .unwrap();

    let frame = responder.await.unwrap();
    assert_eq!(
        frame.get("cmd").and_then(Value::as_str),
        Some("aibot_send_msg")
    );
    assert_eq!(
        frame
            .pointer("/body/markdown/content")
            .and_then(Value::as_str),
        Some("hello proactive")
    );
}

#[tokio::test]
async fn command_response_resolves_waiter_successfully() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (waiter, rx) = tokio::sync::oneshot::channel();
    channel
        .pending_responses
        .lock()
        .await
        .insert("req-ok".to_string(), waiter);

    assert!(
        channel
            .maybe_handle_command_response(&serde_json::json!({
                "headers": { "req_id": "req-ok" },
                "errcode": 0,
                "errmsg": "ok"
            }))
            .await
    );
    assert!(rx.await.unwrap().is_ok());
}

#[tokio::test]
async fn command_response_resolves_waiter_failure() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (waiter, rx) = tokio::sync::oneshot::channel();
    channel
        .pending_responses
        .lock()
        .await
        .insert("req-fail".to_string(), waiter);

    assert!(
        channel
            .maybe_handle_command_response(&serde_json::json!({
                "headers": { "req_id": "req-fail" },
                "errcode": 93001,
                "errmsg": "session not allowed"
            }))
            .await
    );
    let err = rx.await.unwrap().unwrap_err().to_string();
    assert!(err.contains("errcode=93001"));
    assert!(err.contains("session not allowed"));
}

#[tokio::test]
async fn handle_ws_message_consumes_command_ack_without_forwarding() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (waiter, ack_rx) = tokio::sync::oneshot::channel();
    channel
        .pending_responses
        .lock()
        .await
        .insert("req-ack".to_string(), waiter);

    let (tx, mut rx) = mpsc::channel::<ChannelMessage>(1);
    let should_reconnect = channel
        .handle_ws_message(
            serde_json::json!({
                "cmd": "aibot_respond_msg",
                "headers": { "req_id": "req-ack" },
                "errcode": 0,
                "errmsg": "ok"
            }),
            &tx,
        )
        .await;

    assert!(!should_reconnect);
    assert!(ack_rx.await.unwrap().is_ok());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "command ack must not be forwarded as an inbound channel message"
    );
}

#[tokio::test]
async fn clear_command_forwards_runtime_new_session_without_immediate_ws_reply() {
    let mut config = test_wecom_ws_config();
    config.allowed_users = vec!["zeroclaw_user".to_string()];
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (ws_tx, mut ws_rx) = mpsc::channel::<WsOutbound>(1);
    *channel.ws_tx.lock().await = Some(ws_tx);

    let (tx, mut rx) = mpsc::channel::<ChannelMessage>(1);
    channel
        .handle_msg_callback(
            serde_json::json!({
                "headers": { "req_id": "req-clear" },
                "body": {
                    "msgtype": "text",
                    "msgid": "msg-clear",
                    "chattype": "single",
                    "from": { "userid": "zeroclaw_user" },
                    "text": { "content": "/clear" }
                }
            }),
            &tx,
        )
        .await;

    let forwarded = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .expect("clear command should be forwarded promptly")
        .expect("clear command should produce a framework message");
    assert_eq!(forwarded.content, "/new");
    assert_eq!(forwarded.thread_ts.as_deref(), Some("req-clear"));

    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws_rx.recv())
            .await
            .is_err(),
        "clear command should not emit an immediate websocket reply"
    );
}

#[tokio::test]
async fn clear_command_ws_dispatch_does_not_block_when_framework_queue_is_full() {
    let mut config = test_wecom_ws_config();
    config.allowed_users = vec!["zeroclaw_user".to_string()];
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (tx, mut rx) = mpsc::channel::<ChannelMessage>(1);
    tx.send(ChannelMessage::new(
        "prefill-clear",
        "tester",
        "user--zeroclaw_user",
        "prefill",
        "wecom_ws",
        bytes_timestamp_now(),
    ))
    .await
    .unwrap();

    let should_reconnect = tokio::time::timeout(
        Duration::from_millis(100),
        channel.handle_ws_message(
            serde_json::json!({
                "cmd": "aibot_msg_callback",
                "headers": { "req_id": "req-clear-dispatch" },
                "body": {
                    "msgtype": "text",
                    "msgid": "msg-clear-dispatch",
                    "chattype": "single",
                    "from": { "userid": "zeroclaw_user" },
                    "text": { "content": "/clear" }
                }
            }),
            &tx,
        ),
    )
    .await
    .expect("clear dispatch should not block the websocket loop");

    assert!(!should_reconnect);

    let first = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .expect("prefilled framework message should be readable")
        .expect("prefilled framework message should exist");
    assert_eq!(first.id, "prefill-clear");

    let forwarded = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .expect("clear command should forward once queue space is available")
        .expect("clear command should produce a framework message");
    assert_eq!(forwarded.content, "/new");
    assert_eq!(forwarded.thread_ts.as_deref(), Some("req-clear-dispatch"));
}

#[tokio::test]
async fn unauthorized_group_message_replies_with_chatid_and_does_not_forward() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (ws_tx, mut ws_rx) = mpsc::channel::<WsOutbound>(4);
    *channel.ws_tx.lock().await = Some(ws_tx);

    let responder_channel = channel.clone();
    let responder = zeroclaw_spawn::spawn!(async move {
        let Some(WsOutbound::Frame(frame)) = ws_rx.recv().await else {
            panic!("expected access-denied response frame");
        };
        let req_id = frame
            .get("headers")
            .and_then(|headers| headers.get("req_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let content = frame
            .pointer("/body/stream/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        responder_channel
            .maybe_handle_command_response(&serde_json::json!({
                "headers": { "req_id": req_id },
                "errcode": 0,
                "errmsg": "ok"
            }))
            .await;
        content
    });

    let (tx, mut rx) = mpsc::channel::<ChannelMessage>(1);
    channel
        .handle_msg_callback(
            serde_json::json!({
                "headers": { "req_id": "req-denied" },
                "body": {
                    "msgtype": "text",
                    "msgid": "msg-denied",
                    "chattype": "group",
                    "chatid": "zeroclaw_group",
                    "from": { "userid": "zeroclaw_user" },
                    "text": { "content": "@bot hello" }
                }
            }),
            &tx,
        )
        .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "unauthorized message must not reach framework"
    );

    let denied = responder.await.unwrap();
    assert!(denied.contains("zeroclaw_group"));
    assert!(denied.contains("zeroclaw_user"));
    assert!(denied.contains("allowed_groups"));
}

#[tokio::test]
async fn unauthorized_message_ws_dispatch_returns_without_waiting_for_ack() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (ws_tx, mut ws_rx) = mpsc::channel::<WsOutbound>(4);
    *channel.ws_tx.lock().await = Some(ws_tx);

    let (tx, mut rx) = mpsc::channel::<ChannelMessage>(1);
    let should_reconnect = tokio::time::timeout(
        Duration::from_millis(100),
        channel.handle_ws_message(
            serde_json::json!({
                "cmd": "aibot_msg_callback",
                "headers": { "req_id": "req-denied-no-ack" },
                "body": {
                    "msgtype": "text",
                    "msgid": "msg-denied-no-ack",
                    "chattype": "single",
                    "from": { "userid": "zeroclaw_user" },
                    "text": { "content": "@bot hello" }
                }
            }),
            &tx,
        ),
    )
    .await
    .expect("access-denied dispatch should not block on websocket ack");

    assert!(!should_reconnect);

    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "unauthorized message must not reach framework"
    );

    let Some(WsOutbound::Frame(frame)) =
        tokio::time::timeout(Duration::from_millis(100), ws_rx.recv())
            .await
            .expect("access-denied reply should be queued promptly")
    else {
        panic!("expected access-denied response frame");
    };

    assert_eq!(
        frame.get("cmd").and_then(Value::as_str),
        Some("aibot_respond_msg")
    );
    assert_eq!(
        frame
            .get("headers")
            .and_then(|headers| headers.get("req_id"))
            .and_then(Value::as_str),
        Some("req-denied-no-ack")
    );
    assert!(
        frame
            .pointer("/body/stream/content")
            .and_then(Value::as_str)
            .is_some_and(|content| content.contains("allowed_users")),
        "access-denied reply should explain how to configure the allowlist"
    );
}

#[tokio::test]
async fn stream_reply_retries_data_version_conflict() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (tx, mut rx) = mpsc::channel::<WsOutbound>(8);
    *channel.ws_tx.lock().await = Some(tx);

    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responder_channel = channel.clone();
    let responder_attempts = Arc::clone(&attempts);
    let responder = zeroclaw_spawn::spawn!(async move {
        while let Some(WsOutbound::Frame(frame)) = rx.recv().await {
            let attempt = responder_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let req_id = frame
                .get("headers")
                .and_then(|headers| headers.get("req_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let errcode = if attempt == 0 { 6000 } else { 0 };
            let errmsg = if errcode == 0 {
                "ok"
            } else {
                "more than one callers at the same time, data version conflict"
            };
            responder_channel
                .maybe_handle_command_response(&serde_json::json!({
                    "headers": { "req_id": req_id },
                    "errcode": errcode,
                    "errmsg": errmsg
                }))
                .await;

            if errcode == 0 {
                break;
            }
        }
    });

    channel
        .ws_send_respond_msg("req-stream", "stream-1", "hello", false)
        .await
        .unwrap();

    responder.await.unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stream_reply_serializes_same_req_id_updates() {
    let config = test_wecom_ws_config();
    let channel = WeComWsChannel::new(&config, Path::new("/tmp")).unwrap();

    let (tx, mut rx) = mpsc::channel::<WsOutbound>(8);
    *channel.ws_tx.lock().await = Some(tx);

    let first_channel = channel.clone();
    let first = zeroclaw_spawn::spawn!(async move {
        first_channel
            .ws_send_respond_msg("req-serial", "stream-1", "first", false)
            .await
    });

    let second_channel = channel.clone();
    let second = zeroclaw_spawn::spawn!(async move {
        second_channel
            .ws_send_respond_msg("req-serial", "stream-1", "second", false)
            .await
    });

    let first_frame = tokio::time::timeout(Duration::from_millis(250), rx.recv())
        .await
        .expect("first frame should arrive")
        .expect("first frame should exist");
    let WsOutbound::Frame(first_frame) = first_frame;
    assert_eq!(
        first_frame
            .get("body")
            .and_then(|body| body.get("stream"))
            .and_then(|stream| stream.get("content"))
            .and_then(Value::as_str),
        Some("first")
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(75), rx.recv())
            .await
            .is_err(),
        "second frame should wait for the first ack"
    );

    channel
        .maybe_handle_command_response(&serde_json::json!({
            "headers": { "req_id": "req-serial" },
            "errcode": 0,
            "errmsg": "ok"
        }))
        .await;
    first.await.unwrap().unwrap();

    let second_frame = tokio::time::timeout(Duration::from_millis(250), rx.recv())
        .await
        .expect("second frame should arrive after first ack")
        .expect("second frame should exist");
    let WsOutbound::Frame(second_frame) = second_frame;
    assert_eq!(
        second_frame
            .get("body")
            .and_then(|body| body.get("stream"))
            .and_then(|stream| stream.get("content"))
            .and_then(Value::as_str),
        Some("second")
    );

    channel
        .maybe_handle_command_response(&serde_json::json!({
            "headers": { "req_id": "req-serial" },
            "errcode": 0,
            "errmsg": "ok"
        }))
        .await;
    second.await.unwrap().unwrap();
}
