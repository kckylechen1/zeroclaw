use super::*;

fn with_bot_open_id(ch: LarkChannel, bot_open_id: &str) -> LarkChannel {
    ch.set_resolved_bot_open_id(Some(bot_open_id.to_string()));
    ch
}

fn resolver_from(peers: Vec<String>) -> Arc<dyn Fn() -> Vec<String> + Send + Sync> {
    Arc::new(move || peers.clone())
}

fn make_channel() -> LarkChannel {
    with_bot_open_id(
        LarkChannel::new(
            "cli_test_app_id".into(),
            "test_app_secret".into(),
            "test_verification_token".into(),
            None,
            "lark_test_alias",
            resolver_from(vec!["ou_testuser123".into()]),
            true,
        ),
        "ou_bot",
    )
}

#[test]
fn lark_channel_name() {
    let ch = make_channel();
    assert_eq!(ch.name(), "lark");
}

#[test]
fn lark_ws_activity_refreshes_heartbeat_watchdog() {
    assert!(should_refresh_last_recv(&WsMsg::Binary(
        vec![1, 2, 3].into()
    )));
    assert!(should_refresh_last_recv(&WsMsg::Ping(vec![9, 9].into())));
    assert!(should_refresh_last_recv(&WsMsg::Pong(vec![8, 8].into())));
}

#[test]
fn lark_ws_non_activity_frames_do_not_refresh_heartbeat_watchdog() {
    assert!(!should_refresh_last_recv(&WsMsg::Text("hello".into())));
    assert!(!should_refresh_last_recv(&WsMsg::Close(None)));
}

#[test]
fn lark_outgoing_media_kind_maps_shared_marker_kinds() {
    assert_eq!(
        LarkOutgoingMediaKind::from_marker_kind("image"),
        Some(LarkOutgoingMediaKind::Image)
    );
    assert_eq!(
        LarkOutgoingMediaKind::from_marker_kind("document"),
        Some(LarkOutgoingMediaKind::File {
            file_type: "stream"
        })
    );
    assert_eq!(
        LarkOutgoingMediaKind::from_marker_kind("video"),
        Some(LarkOutgoingMediaKind::File { file_type: "mp4" })
    );
    assert_eq!(
        LarkOutgoingMediaKind::from_marker_kind("voice"),
        Some(LarkOutgoingMediaKind::File { file_type: "opus" })
    );
    assert_eq!(LarkOutgoingMediaKind::from_marker_kind("embed"), None);
}

#[test]
fn lark_marker_target_accepts_workspace_relative_file() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let file = workspace.path().join("image.png");
    std::fs::write(&file, b"png").expect("write file");

    let resolved =
        validate_lark_marker_target("image.png", Some(workspace.path())).expect("valid target");

    assert_eq!(resolved, file.canonicalize().expect("canonical file"));
}

#[test]
fn lark_marker_target_rejects_workspace_escape() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let file = outside.path().join("secret.txt");
    std::fs::write(&file, b"secret").expect("write outside file");

    let err = validate_lark_marker_target(&file.to_string_lossy(), Some(workspace.path()))
        .expect_err("outside workspace must be refused");

    assert!(
        err.to_string().contains("outside workspace_dir"),
        "expected workspace escape error, got: {err}"
    );
}

#[test]
fn lark_marker_target_rejects_url_schemes() {
    let workspace = tempfile::tempdir().expect("workspace");

    let err = validate_lark_marker_target("https://example.com/image.png", Some(workspace.path()))
        .expect_err("url target must be refused");

    assert!(
        err.to_string().contains("disallowed scheme"),
        "expected scheme error, got: {err}"
    );
}

#[test]
fn lark_group_response_requires_matching_bot_mention_when_ids_available() {
    let mentions = vec![serde_json::json!({
        "id": { "open_id": "ou_other" }
    })];
    assert!(!should_respond_in_group(
        true,
        Some("ou_bot"),
        &mentions,
        &[]
    ));

    let mentions = vec![serde_json::json!({
        "id": { "open_id": "ou_bot" }
    })];
    assert!(should_respond_in_group(
        true,
        Some("ou_bot"),
        &mentions,
        &[]
    ));
}

#[test]
fn lark_group_response_requires_resolved_open_id_when_mention_only_enabled() {
    let mentions = vec![serde_json::json!({
        "id": { "open_id": "ou_any" }
    })];
    assert!(!should_respond_in_group(true, None, &mentions, &[]));
}

#[test]
fn lark_group_response_allows_post_mentions_for_bot_open_id() {
    assert!(should_respond_in_group(
        true,
        Some("ou_bot"),
        &[],
        &[String::from("ou_bot")]
    ));
}

#[test]
fn lark_should_refresh_token_on_http_401() {
    let body = serde_json::json!({ "code": 0 });
    assert!(should_refresh_lark_tenant_token(
        reqwest::StatusCode::UNAUTHORIZED,
        &body
    ));
}

#[test]
fn lark_should_refresh_token_on_body_code_99991663() {
    let body = serde_json::json!({
        "code": LARK_INVALID_ACCESS_TOKEN_CODE,
        "msg": "Invalid access token for authorization."
    });
    assert!(should_refresh_lark_tenant_token(
        reqwest::StatusCode::OK,
        &body
    ));
}

#[test]
fn lark_should_not_refresh_token_on_success_body() {
    let body = serde_json::json!({ "code": 0, "msg": "ok" });
    assert!(!should_refresh_lark_tenant_token(
        reqwest::StatusCode::OK,
        &body
    ));
}

#[test]
fn lark_extract_token_ttl_seconds_supports_expire_and_expires_in() {
    let body_expire = serde_json::json!({ "expire": 7200 });
    let body_expires_in = serde_json::json!({ "expires_in": 3600 });
    let body_missing = serde_json::json!({});
    assert_eq!(extract_lark_token_ttl_seconds(&body_expire), 7200);
    assert_eq!(extract_lark_token_ttl_seconds(&body_expires_in), 3600);
    assert_eq!(
        extract_lark_token_ttl_seconds(&body_missing),
        LARK_DEFAULT_TOKEN_TTL.as_secs()
    );
}

#[test]
fn lark_next_token_refresh_deadline_reserves_refresh_skew() {
    let now = Instant::now();
    let regular = next_token_refresh_deadline(now, 7200);
    let short_ttl = next_token_refresh_deadline(now, 60);

    assert_eq!(regular.duration_since(now), Duration::from_secs(7080));
    assert_eq!(short_ttl.duration_since(now), Duration::from_secs(1));
}

#[test]
fn lark_ensure_send_success_rejects_non_zero_code() {
    let ok = serde_json::json!({ "code": 0 });
    let bad = serde_json::json!({ "code": 12345, "msg": "bad request" });

    assert!(ensure_lark_send_success(reqwest::StatusCode::OK, &ok, "test").is_ok());
    assert!(ensure_lark_send_success(reqwest::StatusCode::OK, &bad, "test").is_err());
}

#[test]
fn lark_user_allowed_exact() {
    let ch = make_channel();
    assert!(ch.is_user_allowed("ou_testuser123"));
    assert!(!ch.is_user_allowed("ou_other"));
}

#[test]
fn lark_user_allowed_wildcard() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    assert!(ch.is_user_allowed("ou_anyone"));
}

#[test]
fn lark_user_denied_empty() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec![]),
        true,
    );
    assert!(!ch.is_user_allowed("ou_anyone"));
}

#[tokio::test]
async fn lark_parse_challenge() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "challenge": "abc123",
        "token": "test_verification_token",
        "type": "url_verification"
    });
    // Challenge payloads should not produce messages
    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_valid_text_message() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": {
                    "open_id": "ou_testuser123"
                }
            },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"Hello ZeroClaw!\"}",
                "chat_id": "oc_chat123",
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "Hello ZeroClaw!");
    assert_eq!(msgs[0].sender, "oc_chat123");
    assert_eq!(msgs[0].channel, "lark");
    assert_eq!(msgs[0].timestamp, 1_699_999_999);
}

#[tokio::test]
async fn lark_parse_unauthorized_user() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_unauthorized" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"spam\"}",
                "chat_id": "oc_chat",
                "create_time": "1000"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_unsupported_message_type_skipped() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "sticker",
                "content": "{}",
                "chat_id": "oc_chat"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[test]
fn parse_list_content_flat_items() {
    // Flat structure: items is an array of arrays of inline elements
    let content =
        r#"{"items":[[{"tag":"text","text":"first item"}],[{"tag":"text","text":"second item"}]]}"#;
    let result = parse_list_content(content).unwrap();
    assert_eq!(result, "- first item\n- second item");
}

#[test]
fn parse_list_content_nested_children() {
    // Nested structure: items are objects with content + children
    let content = r#"{"items":[{"content":[[{"tag":"text","text":"parent"}]],"children":[{"content":[[{"tag":"text","text":"child"}]]}]}]}"#;
    let result = parse_list_content(content).unwrap();
    assert_eq!(result, "- parent\n  - child");
}

#[test]
fn parse_list_content_with_links() {
    let content = r#"{"items":[[{"tag":"text","text":"see "},{"tag":"a","text":"docs","href":"https://example.com"}]]}"#;
    let result = parse_list_content(content).unwrap();
    assert_eq!(result, "- see docs");
}

#[test]
fn parse_list_content_empty_returns_none() {
    let content = r#"{"items":[]}"#;
    assert!(parse_list_content(content).is_none());
}

#[test]
fn parse_list_content_invalid_json_returns_none() {
    assert!(parse_list_content("not json").is_none());
}

#[tokio::test]
async fn lark_parse_list_message_type() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "list",
                "content": "{\"items\":[[{\"tag\":\"text\",\"text\":\"buy milk\"}],[{\"tag\":\"text\",\"text\":\"buy eggs\"}]]}",
                "chat_id": "oc_chat",
                "create_time": "1000"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].content.contains("buy milk"));
    assert!(msgs[0].content.contains("buy eggs"));
}

#[tokio::test]
async fn lark_parse_image_missing_key_skipped() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "image",
                "content": "{}",
                "chat_id": "oc_chat"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_file_missing_key_skipped() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "file",
                "content": "{}",
                "chat_id": "oc_chat"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_empty_text_skipped() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"\"}",
                "chat_id": "oc_chat"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_wrong_event_type() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": { "event_type": "im.chat.disbanded_v1" },
        "event": {}
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_missing_sender() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"hello\"}",
                "chat_id": "oc_chat"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_unicode_message() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"Hello world 🌍\"}",
                "chat_id": "oc_chat",
                "create_time": "1000"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "Hello world 🌍");
}

#[tokio::test]
async fn lark_parse_missing_event() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_invalid_content_json() {
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "not valid json",
                "chat_id": "oc_chat"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert!(msgs.is_empty());
}

#[test]
fn lark_config_serde() {
    use zeroclaw_config::schema::{LarkConfig, LarkReceiveMode};
    let lc = LarkConfig {
        enabled: true,
        app_id: "cli_app123".into(),
        app_secret: "secret456".into(),
        encrypt_key: None,
        verification_token: Some("vtoken789".into()),
        mention_only: false,
        use_feishu: false,
        receive_mode: LarkReceiveMode::default(),
        port: None,
        proxy_url: None,
        excluded_tools: vec![],
        approval_timeout_secs: 300,
        per_user_session: false,
        ack_reactions: None,
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: 1000,
    };
    let json = serde_json::to_string(&lc).unwrap();
    let parsed: LarkConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.app_id, "cli_app123");
    assert_eq!(parsed.app_secret, "secret456");
    assert_eq!(parsed.verification_token.as_deref(), Some("vtoken789"));
}

#[test]
fn lark_config_toml_roundtrip() {
    use zeroclaw_config::schema::{LarkConfig, LarkReceiveMode};
    let lc = LarkConfig {
        enabled: true,
        app_id: "app".into(),
        app_secret: "secret".into(),
        encrypt_key: None,
        verification_token: Some("tok".into()),
        mention_only: false,
        use_feishu: false,
        receive_mode: LarkReceiveMode::Webhook,
        port: Some(9898),
        proxy_url: None,
        excluded_tools: vec![],
        approval_timeout_secs: 300,
        per_user_session: false,
        ack_reactions: None,
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: 1000,
    };
    let toml_str = toml::to_string(&lc).unwrap();
    let parsed: LarkConfig = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.app_id, "app");
    assert_eq!(parsed.verification_token.as_deref(), Some("tok"));
}

#[test]
fn lark_config_defaults_optional_fields() {
    use zeroclaw_config::schema::{LarkConfig, LarkReceiveMode};
    let json = r#"{"app_id":"a","app_secret":"s"}"#;
    let parsed: LarkConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.verification_token.is_none());
    assert!(!parsed.mention_only);
    assert_eq!(parsed.receive_mode, LarkReceiveMode::Websocket);
    assert!(parsed.port.is_none());
}

#[test]
fn lark_from_config_preserves_mode_and_region() {
    use zeroclaw_config::schema::{LarkConfig, LarkReceiveMode};

    let cfg = LarkConfig {
        enabled: true,
        app_id: "cli_app123".into(),
        app_secret: "secret456".into(),
        encrypt_key: None,
        verification_token: Some("vtoken789".into()),
        mention_only: false,
        use_feishu: false,
        receive_mode: LarkReceiveMode::Webhook,
        port: Some(9898),
        proxy_url: None,
        excluded_tools: vec![],
        approval_timeout_secs: 300,
        per_user_session: false,
        ack_reactions: None,
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: 1000,
    };

    let ch = LarkChannel::from_config(&cfg, "lark_test_alias", resolver_from(vec!["*".into()]));

    assert_eq!(ch.api_base(), LARK_BASE_URL);
    assert_eq!(ch.ws_base(), LARK_WS_BASE_URL);
    assert_eq!(ch.receive_mode, LarkReceiveMode::Webhook);
    assert_eq!(ch.port, Some(9898));
}

#[test]
fn lark_from_config_with_use_feishu_routes_to_feishu() {
    use zeroclaw_config::schema::{LarkConfig, LarkReceiveMode};

    let cfg = LarkConfig {
        enabled: true,
        app_id: "cli_feishu_app123".into(),
        app_secret: "secret456".into(),
        encrypt_key: None,
        verification_token: Some("vtoken789".into()),
        mention_only: false,
        use_feishu: true,
        receive_mode: LarkReceiveMode::Webhook,
        port: Some(9898),
        proxy_url: None,
        excluded_tools: vec![],
        approval_timeout_secs: 300,
        per_user_session: false,
        ack_reactions: None,
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: 1000,
    };

    let ch = LarkChannel::from_config(&cfg, "feishu_test_alias", resolver_from(vec!["*".into()]));

    assert_eq!(ch.api_base(), FEISHU_BASE_URL);
    assert_eq!(ch.ws_base(), FEISHU_WS_BASE_URL);
    assert_eq!(ch.name(), "lark");
}

#[test]
fn lark_with_approval_timeout_secs_propagates_value() {
    use zeroclaw_config::schema::{LarkConfig, LarkReceiveMode};

    let cfg = LarkConfig {
        enabled: true,
        app_id: "cli_app123".into(),
        app_secret: "secret456".into(),
        encrypt_key: None,
        verification_token: Some("vtoken789".into()),
        mention_only: false,
        use_feishu: false,
        receive_mode: LarkReceiveMode::Websocket,
        port: None,
        proxy_url: None,
        excluded_tools: vec![],
        approval_timeout_secs: 456,
        per_user_session: false,
        ack_reactions: None,
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: 1000,
    };

    let ch = LarkChannel::from_config(&cfg, "lark_test_alias", resolver_from(vec!["*".into()]))
        .with_approval_timeout_secs(cfg.approval_timeout_secs);

    assert_eq!(ch.approval_timeout_secs, 456);
}

#[test]
fn lark_with_per_user_session_propagates_value() {
    let ch_on = make_channel().with_per_user_session(true);
    assert!(ch_on.per_user_session);
    let ch_off = make_channel().with_per_user_session(false);
    assert!(!ch_off.per_user_session);
}

#[test]
fn supports_draft_updates_reflects_stream_mode() {
    let off = make_channel();
    assert!(!off.supports_draft_updates());

    let partial = make_channel().with_streaming(StreamMode::Partial, 500);
    assert!(partial.supports_draft_updates());
}

#[tokio::test]
async fn update_draft_rate_limits_within_interval() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-rate",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    let patch_mock = Mock::given(method("PATCH"))
        .and(path_regex("/im/v1/messages/om_draft_rl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 0 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel().with_streaming(StreamMode::Partial, 5_000);
    ch.api_base_override = Some(server.uri());

    ch.update_draft("oc_chat1", "om_draft_rl", "first")
        .await
        .expect("first update_draft ok");
    ch.update_draft("oc_chat1", "om_draft_rl", "second")
        .await
        .expect("second update_draft ok");

    drop(patch_mock);
}

#[tokio::test]
async fn update_draft_proceeds_after_interval() {
    use std::time::Duration as StdDuration;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-proceed",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    let patch_mock = Mock::given(method("PATCH"))
        .and(path_regex("/im/v1/messages/om_draft_go"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 0 })))
        .expect(2)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel().with_streaming(StreamMode::Partial, 50);
    ch.api_base_override = Some(server.uri());

    ch.update_draft("oc_chat1", "om_draft_go", "first")
        .await
        .expect("first update_draft ok");
    tokio::time::sleep(StdDuration::from_millis(80)).await;
    ch.update_draft("oc_chat1", "om_draft_go", "second")
        .await
        .expect("second update_draft ok");

    drop(patch_mock);
}

#[test]
fn lark_resolve_sender_respects_per_user_session_flag() {
    let mut ch = make_channel();

    assert!(!ch.per_user_session);
    assert_eq!(ch.resolve_sender("oc_chat", Some("ou_user")), "oc_chat");
    assert_eq!(ch.resolve_sender("oc_chat", None), "oc_chat");
    assert_eq!(ch.resolve_sender("oc_chat", Some("")), "oc_chat");

    ch.per_user_session = true;
    assert_eq!(ch.resolve_sender("oc_chat", Some("ou_user")), "ou_user");
    assert_eq!(ch.resolve_sender("oc_chat", None), "oc_chat");
    assert_eq!(ch.resolve_sender("oc_chat", Some("")), "oc_chat");
}

#[tokio::test]
async fn lark_parse_fallback_sender_to_open_id() {
    // When chat_id is missing, sender should fall back to open_id
    let ch = LarkChannel::new(
        "id".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        true,
    );
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"hello\"}",
                "create_time": "1000"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].sender, "ou_user");
}

#[tokio::test]
async fn lark_parse_group_message_requires_bot_mention_when_enabled() {
    let ch = with_bot_open_id(
        LarkChannel::new(
            "cli_app123".into(),
            "secret".into(),
            "token".into(),
            None,
            "lark_test_alias",
            resolver_from(vec!["*".into()]),
            true,
        ),
        "ou_bot_123",
    );

    let no_mention_payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"hello\"}",
                "chat_type": "group",
                "chat_id": "oc_chat",
                "mentions": []
            }
        }
    });
    assert!(ch.parse_event_payload(&no_mention_payload).await.is_empty());

    let wrong_mention_payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"hello\"}",
                "chat_type": "group",
                "chat_id": "oc_chat",
                "mentions": [{ "id": { "open_id": "ou_other" } }]
            }
        }
    });
    assert!(
        ch.parse_event_payload(&wrong_mention_payload)
            .await
            .is_empty()
    );

    let bot_mention_payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"hello\"}",
                "chat_type": "group",
                "chat_id": "oc_chat",
                "mentions": [{ "id": { "open_id": "ou_bot_123" } }]
            }
        }
    });
    assert_eq!(ch.parse_event_payload(&bot_mention_payload).await.len(), 1);
}

#[tokio::test]
async fn lark_parse_group_post_message_accepts_at_when_top_level_mentions_empty() {
    let ch = with_bot_open_id(
        LarkChannel::new(
            "cli_app123".into(),
            "secret".into(),
            "token".into(),
            None,
            "lark_test_alias",
            resolver_from(vec!["*".into()]),
            true,
        ),
        "ou_bot_123",
    );

    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "post",
                "chat_type": "group",
                "chat_id": "oc_chat",
                "mentions": [],
                "content": "{\"zh_cn\":{\"title\":\"\",\"content\":[[{\"tag\":\"at\",\"user_id\":\"ou_bot_123\",\"user_name\":\"Bot\"},{\"tag\":\"text\",\"text\":\" hi\"}]]}}"
            }
        }
    });

    assert_eq!(ch.parse_event_payload(&payload).await.len(), 1);
}

#[tokio::test]
async fn lark_parse_post_message_accepts_md_tag_text_content() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_testuser123" } },
            "message": {
                "message_type": "post",
                "chat_type": "p2p",
                "chat_id": "oc_chat",
                "mentions": [],
                "content": "{\"zh_cn\":{\"title\":\"\",\"content\":[[{\"tag\":\"md\",\"text\":\"* 1\\n* 2\"}]]}}"
            }
        }
    });

    let msgs = ch.parse_event_payload(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "* 1\n* 2");
}

#[tokio::test]
async fn lark_parse_group_message_allows_without_mention_when_disabled() {
    let ch = LarkChannel::new(
        "cli_app123".into(),
        "secret".into(),
        "token".into(),
        None,
        "lark_test_alias",
        resolver_from(vec!["*".into()]),
        false,
    );

    let payload = serde_json::json!({
        "header": { "event_type": "im.message.receive_v1" },
        "event": {
            "sender": { "sender_id": { "open_id": "ou_user" } },
            "message": {
                "message_type": "text",
                "content": "{\"text\":\"hello\"}",
                "chat_type": "group",
                "chat_id": "oc_chat",
                "mentions": []
            }
        }
    });

    assert_eq!(ch.parse_event_payload(&payload).await.len(), 1);
}

#[test]
fn lark_reaction_url_matches_region() {
    let ch_lark = make_channel();
    assert_eq!(
        ch_lark.message_reaction_url("om_test_message_id"),
        "https://open.larksuite.com/open-apis/im/v1/messages/om_test_message_id/reactions"
    );

    let feishu_cfg = zeroclaw_config::schema::LarkConfig {
        enabled: true,
        app_id: "cli_app123".into(),
        app_secret: "secret456".into(),
        encrypt_key: None,
        verification_token: Some("vtoken789".into()),
        mention_only: false,
        use_feishu: true,
        receive_mode: zeroclaw_config::schema::LarkReceiveMode::Webhook,
        port: Some(9898),
        proxy_url: None,
        excluded_tools: vec![],
        approval_timeout_secs: 300,
        per_user_session: false,
        ack_reactions: None,
        stream_mode: StreamMode::default(),
        draft_update_interval_ms: 1000,
    };
    let ch_feishu = LarkChannel::from_config(
        &feishu_cfg,
        "feishu_test_alias",
        resolver_from(vec!["*".into()]),
    );
    assert_eq!(
        ch_feishu.message_reaction_url("om_test_message_id"),
        "https://open.feishu.cn/open-apis/im/v1/messages/om_test_message_id/reactions"
    );
}

#[test]
fn lark_image_max_bytes_is_10_mib() {
    assert_eq!(LARK_IMAGE_MAX_BYTES, 10 * 1024 * 1024);
}

#[test]
fn lark_file_download_url_matches_region() {
    let ch = make_channel();
    assert_eq!(
        ch.file_download_url("om_msg123", "file_abc"),
        "https://open.larksuite.com/open-apis/im/v1/messages/om_msg123/resources/file_abc?type=file"
    );
}

#[test]
fn lark_detect_image_mime_from_magic_bytes() {
    let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    assert_eq!(
        lark_detect_image_mime(None, &png).as_deref(),
        Some("image/png")
    );

    let jpeg = [0xff, 0xd8, 0xff, 0xe0];
    assert_eq!(
        lark_detect_image_mime(None, &jpeg).as_deref(),
        Some("image/jpeg")
    );

    let gif = b"GIF89a...";
    assert_eq!(
        lark_detect_image_mime(None, gif).as_deref(),
        Some("image/gif")
    );

    // Unknown bytes should fall back to content-type header
    let unknown = [0x00, 0x01, 0x02];
    assert_eq!(
        lark_detect_image_mime(Some("image/webp"), &unknown).as_deref(),
        Some("image/webp")
    );

    // Non-image content-type should be rejected
    assert_eq!(lark_detect_image_mime(Some("text/html"), &unknown), None);

    // No info at all should return None
    assert_eq!(lark_detect_image_mime(None, &unknown), None);
}

#[test]
fn lark_is_text_filename_recognizes_common_extensions() {
    assert!(lark_is_text_filename("script.py"));
    assert!(lark_is_text_filename("config.toml"));
    assert!(lark_is_text_filename("data.csv"));
    assert!(lark_is_text_filename("README.md"));
    assert!(!lark_is_text_filename("image.png"));
    assert!(!lark_is_text_filename("archive.zip"));
    assert!(!lark_is_text_filename("binary.exe"));
}

#[test]
fn lark_inline_text_file_preview_truncates_on_utf8_boundary() {
    let prefix = "a".repeat(49_999);
    let text = format!("{prefix}{}tail", "😀");
    let preview = lark_inline_text_file_preview(Cow::Borrowed(&text));

    assert_eq!(preview, format!("{prefix}...\n[truncated]"));
}

#[test]
fn build_interactive_card_body_produces_correct_structure() {
    let body = build_interactive_card_body("oc_chat123", "**Hello** world");
    assert_eq!(body["receive_id"], "oc_chat123");
    assert_eq!(body["msg_type"], "interactive");

    let content: serde_json::Value =
        serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["schema"], "2.0");
    let elements = content["body"]["elements"].as_array().unwrap();
    assert_eq!(elements.len(), 1);
    assert_eq!(elements[0]["tag"], "markdown");
    assert_eq!(elements[0]["content"], "**Hello** world");
}

#[test]
fn build_card_content_produces_valid_json() {
    let content = build_card_content("# Title\n\n**Bold** text");
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["schema"], "2.0");
    assert_eq!(parsed["body"]["elements"][0]["tag"], "markdown");
    assert_eq!(
        parsed["body"]["elements"][0]["content"],
        "# Title\n\n**Bold** text"
    );
}

#[test]
fn split_markdown_chunks_single_chunk_for_small_content() {
    let text = "Hello world";
    let chunks = split_markdown_chunks(text, LARK_CARD_MARKDOWN_MAX_BYTES);
    assert_eq!(chunks, vec!["Hello world"]);
}

#[test]
fn split_markdown_chunks_splits_on_newline_boundaries() {
    let line = "abcdefghij\n"; // 11 bytes per line
    let text = line.repeat(10); // 110 bytes total
    let chunks = split_markdown_chunks(&text, 33); // ~3 lines per chunk
    assert_eq!(chunks.len(), 4);
    for chunk in &chunks[..3] {
        assert!(chunk.len() <= 33);
        assert!(chunk.ends_with('\n'));
    }
}

#[test]
fn split_markdown_chunks_handles_no_newlines() {
    let text = "a".repeat(100);
    let chunks = split_markdown_chunks(&text, 30);
    assert!(chunks.len() > 1);
    let reassembled: String = chunks.concat();
    assert_eq!(reassembled, text);
}

#[test]
fn split_markdown_chunks_exact_boundary() {
    let text = "abc";
    let chunks = split_markdown_chunks(text, 3);
    assert_eq!(chunks, vec!["abc"]);
}

#[test]
fn lark_manager_none_when_transcription_not_configured() {
    let ch = make_channel();
    assert!(ch.transcription_manager.is_none());
}

#[test]
fn lark_manager_none_when_disabled() {
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: false,
        ..Default::default()
    };
    let ch = make_channel().with_transcription(tc);
    assert!(ch.transcription_manager.is_none());
}

#[test]
fn lark_manager_none_and_warn_on_init_failure() {
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        api_key: Some(String::new()),
        ..Default::default()
    };
    let ch = make_channel().with_transcription(tc);
    assert!(ch.transcription_manager.is_none());
    assert!(ch.transcription.is_some());
}

#[test]
fn lark_audio_extensionless_file_key_falls_back_to_m4a() {
    assert_eq!(inferred_audio_filename("abc123"), "voice.m4a");
    assert_eq!(inferred_audio_filename("file_without_ext"), "voice.m4a");
}

#[test]
fn lark_audio_extensionless_file_key_preserves_existing_extension() {
    assert_eq!(inferred_audio_filename("abc.m4a"), "abc.m4a");
    assert_eq!(inferred_audio_filename("voice.ogg"), "voice.ogg");
    assert_eq!(inferred_audio_filename("audio.mp3"), "audio.mp3");
    assert_eq!(inferred_audio_filename("note.aac"), "note.aac");
    assert_eq!(inferred_audio_filename("file.wav"), "file.wav");
}

#[tokio::test]
async fn lark_parse_audio_message_type_skipped_without_manager() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": {
                    "open_id": "ou_testuser123"
                }
            },
            "message": {
                "message_id": "om_audio123",
                "message_type": "audio",
                "content": "{\"file_key\":\"audio_file_key\"}",
                "chat_id": "oc_chat123",
                "chat_type": "p2p",
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload_async(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_parse_text_still_works_via_async_path() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": {
                    "open_id": "ou_testuser123"
                }
            },
            "message": {
                "message_id": "om_text123",
                "message_type": "text",
                "content": "{\"text\":\"Hello async!\"}",
                "chat_id": "oc_chat123",
                "chat_type": "p2p",
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload_async(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "Hello async!");
}

#[tokio::test]
async fn lark_audio_group_without_mention_skips_before_download() {
    let ch = make_channel();
    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": {
                    "open_id": "ou_testuser123"
                }
            },
            "message": {
                "message_id": "om_audio_group",
                "message_type": "audio",
                "content": "{\"file_key\":\"audio_file_key\"}",
                "chat_id": "oc_group123",
                "chat_type": "group",
                "mentions": [],
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload_async(&payload).await;
    assert!(msgs.is_empty());
}

#[test]
fn lark_feishu_audio_uses_feishu_api_base() {
    let ch = LarkChannel::new_with_platform(
        "app_id".into(),
        "secret".into(),
        "token".into(),
        None,
        "feishu_test_alias",
        resolver_from(vec![]),
        false,
        LarkPlatform::Feishu,
    );
    assert_eq!(ch.api_base(), FEISHU_BASE_URL);
}

#[tokio::test]
async fn lark_audio_file_key_missing_returns_none() {
    let ch = make_channel();
    let tc = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        local_whisper: Some(zeroclaw_config::schema::LocalWhisperConfig {
            url: "http://localhost:0/v1/transcribe".to_string(),
            bearer_token: Some("unused".to_string()),
            max_audio_bytes: 10 * 1024 * 1024,
            timeout_secs: 30,
        }),
        ..Default::default()
    };
    let ch = ch.with_transcription(tc);
    let manager = ch.transcription_manager.as_deref().unwrap();

    let result = ch
        .try_transcribe_audio_message("om_123", "{}", manager)
        .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn lark_audio_skips_when_manager_none() {
    let ch = make_channel();
    assert!(ch.transcription_manager.is_none());

    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": { "open_id": "ou_testuser123" }
            },
            "message": {
                "message_id": "om_audio_1",
                "message_type": "audio",
                "content": "{\"file_key\":\"fk_abc123\"}",
                "chat_id": "oc_chat1",
                "chat_type": "p2p",
                "mentions": [],
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload_async(&payload).await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn lark_audio_routes_through_transcription_manager() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Mock the tenant access token endpoint
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "test-tenant-token",
            "expire": 7200
        })))
        .mount(&mock_server)
        .await;

    // Mock the audio resource download endpoint
    Mock::given(method("GET"))
        .and(path_regex("/im/v1/messages/.+/resources/.+"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 128]))
        .mount(&mock_server)
        .await;

    // Mock whisper transcription endpoint
    let whisper_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/v1/transcribe"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"text": "test transcript"})),
        )
        .mount(&whisper_server)
        .await;

    let config = zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        local_whisper: Some(zeroclaw_config::schema::LocalWhisperConfig {
            url: format!("{}/v1/transcribe", whisper_server.uri()),
            bearer_token: Some("test-token".to_string()),
            max_audio_bytes: 10 * 1024 * 1024,
            timeout_secs: 30,
        }),
        ..Default::default()
    };

    let mut ch = make_channel();
    ch.api_base_override = Some(mock_server.uri());
    let ch = ch.with_transcription(config);

    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": { "open_id": "ou_testuser123" }
            },
            "message": {
                "message_id": "om_audio_2",
                "message_type": "audio",
                "content": "{\"file_key\":\"fk_abc123\"}",
                "chat_id": "oc_chat1",
                "chat_type": "p2p",
                "mentions": [],
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload_async(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "test transcript");
}

#[tokio::test]
async fn lark_audio_token_refresh_on_invalid_token_response() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    // Token endpoint always returns valid token
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "refreshed-token",
            "expire": 7200
        })))
        .mount(&mock_server)
        .await;

    // Resource endpoint: first call returns 401, second returns audio bytes
    Mock::given(method("GET"))
        .and(path_regex("/im/v1/messages/.+/resources/.+"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "code": 99_991_663,
            "msg": "token invalid"
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/im/v1/messages/.+/resources/.+"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 64]))
        .mount(&mock_server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(mock_server.uri());

    let result = ch.download_audio_resource("om_msg_1", "fk_audio_key").await;
    assert!(result.is_ok());
    let (bytes, filename) = result.unwrap();
    assert_eq!(bytes.len(), 64);
    assert_eq!(filename, "voice.m4a");
}

// ─────────────────────────────────────────────────────────────────────
// Card 2.0 approval card tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn build_approval_card_contains_all_three_buttons() {
    let card = build_approval_card("test-id", "shell", "rm -rf /tmp/foo");

    // Card 2.0 schema lock — guard against future regressions where the
    // send-side schema drifts back to 1.0 (which Feishu's PATCH endpoint
    // silently refuses to re-render after the click).
    assert_eq!(
        card.get("schema").and_then(|v| v.as_str()),
        Some("2.0"),
        "approval card must use Card JSON 2.0 schema"
    );

    let columns = card
        .pointer("/body/elements/1/columns")
        .and_then(|v| v.as_array())
        .expect("column_set with columns missing");
    assert_eq!(
        columns.len(),
        3,
        "expected 3 button columns (Approve/Deny/Always)"
    );

    let decisions: Vec<&str> = columns
        .iter()
        .filter_map(|c| {
            c.pointer("/elements/0/behaviors/0/value/decision")
                .and_then(|d| d.as_str())
        })
        .collect();
    assert_eq!(decisions, vec!["approve", "deny", "always"]);
}

#[test]
fn build_approval_card_round_trips_approval_id_in_all_buttons() {
    let card = build_approval_card("approval-abc-123", "tool", "args");
    let columns = card["body"]["elements"][1]["columns"]
        .as_array()
        .expect("columns array");
    for column in columns {
        assert_eq!(
            column["elements"][0]["behaviors"][0]["value"]["approval_id"],
            "approval-abc-123"
        );
    }
}

#[test]
fn build_approval_card_and_resolved_card_share_schema_version() {
    use zeroclaw_api::channel::ChannelApprovalResponse;

    let send_card = build_approval_card("id", "shell", "args");
    let patch_card =
        build_resolved_approval_card("shell", "args", ChannelApprovalResponse::Approve);

    let send_schema = send_card.get("schema").and_then(|v| v.as_str());
    let patch_schema = patch_card.get("schema").and_then(|v| v.as_str());

    assert_eq!(
        send_schema, patch_schema,
        "send-time approval card and PATCH-time resolved card MUST use the same Card JSON schema; \
             Feishu's IM PATCH endpoint silently fails to re-render on the client when send/patch \
             schema versions differ"
    );
    assert_eq!(send_schema, Some("2.0"));
}

#[test]
fn build_resolved_approval_card_uses_decision_specific_banner() {
    use zeroclaw_api::channel::ChannelApprovalResponse;

    for (decision, expected_template, expected_text_fragment) in [
        (ChannelApprovalResponse::Approve, "green", "Approved"),
        (
            ChannelApprovalResponse::AlwaysApprove,
            "green",
            "Approved (always)",
        ),
        (ChannelApprovalResponse::Deny, "red", "Denied"),
    ] {
        let card = build_resolved_approval_card("shell", "args", decision.clone());
        assert_eq!(
            card.pointer("/header/template").and_then(|v| v.as_str()),
            Some(expected_template),
            "decision={decision:?} should use header template {expected_template}"
        );
        let title = card
            .pointer("/header/title/content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            title.contains(expected_text_fragment),
            "decision={decision:?} header title `{title}` should contain `{expected_text_fragment}`"
        );
    }
}

#[test]
fn sanitize_card_action_payload_redacts_sensitive_fields() {
    let raw = serde_json::json!({
        "action": {
            "tag": "button",
            "value": {
                "approval_id": "2ecbcc0f-59f0-4216-ba1c-5b6f4deaf7c7",
                "decision": "approve"
            }
        },
        "context": {
            "open_chat_id": "oc_real_chat_id_LEAKED",
            "open_message_id": "om_real_msg_id_LEAKED"
        },
        "host": "im_message",
        "operator": {
            "open_id": "ou_real_user_id_LEAKED",
            "tenant_key": "real_tenant_key_LEAKED",
            "union_id": "on_real_union_id_LEAKED",
            "user_id": "real_user_id_LEAKED"
        },
        "token": "c-real_callback_token_LEAKED"
    });

    let sanitized = sanitize_card_action_payload(&raw);
    let dumped = serde_json::to_string(&sanitized).expect("sanitized must serialize");

    for forbidden in [
        "oc_real_chat_id_LEAKED",
        "om_real_msg_id_LEAKED",
        "ou_real_user_id_LEAKED",
        "real_tenant_key_LEAKED",
        "on_real_union_id_LEAKED",
        "real_user_id_LEAKED",
        "c-real_callback_token_LEAKED",
    ] {
        assert!(
            !dumped.contains(forbidden),
            "sanitized payload must not contain raw value {forbidden:?}; got {dumped}"
        );
    }

    assert_eq!(sanitized["token"], "REDACTED_TOKEN");
    assert_eq!(
        sanitized["operator"]["open_id"],
        "REDACTED_OPERATOR_OPEN_ID"
    );
    assert_eq!(
        sanitized["operator"]["union_id"],
        "REDACTED_OPERATOR_UNION_ID"
    );
    assert_eq!(
        sanitized["operator"]["user_id"],
        "REDACTED_OPERATOR_USER_ID"
    );
    assert_eq!(
        sanitized["operator"]["tenant_key"],
        "REDACTED_OPERATOR_TENANT_KEY"
    );
    assert_eq!(
        sanitized["context"]["open_chat_id"],
        "REDACTED_OPEN_CHAT_ID"
    );
    assert_eq!(
        sanitized["context"]["open_message_id"],
        "REDACTED_OPEN_MESSAGE_ID"
    );

    assert_eq!(
        sanitized["action"]["value"]["approval_id"],
        "2ecbcc0f-59f0-4216-ba1c-5b6f4deaf7c7"
    );
    assert_eq!(sanitized["action"]["value"]["decision"], "approve");
    assert_eq!(sanitized["action"]["tag"], "button");
    assert_eq!(sanitized["host"], "im_message");

    assert_eq!(raw["token"], "c-real_callback_token_LEAKED");
    assert_eq!(raw["operator"]["open_id"], "ou_real_user_id_LEAKED");
}

#[test]
fn sanitize_card_action_payload_handles_missing_optional_fields() {
    let raw = serde_json::json!({
        "action": { "value": { "approval_id": "x", "decision": "approve" } }
    });
    let sanitized = sanitize_card_action_payload(&raw);
    assert!(sanitized.get("token").is_none());
    assert!(sanitized.get("operator").is_none());
    assert!(sanitized.get("context").is_none());
    assert_eq!(sanitized["action"]["value"]["decision"], "approve");
}

#[test]
fn sanitize_card_action_payload_redacts_committed_fixtures() {
    let fixtures: [(&str, &str); 3] = [
        (
            "card_action_approve.json",
            include_str!("../../tests/fixtures/lark/card_action_approve.json"),
        ),
        (
            "card_action_deny.json",
            include_str!("../../tests/fixtures/lark/card_action_deny.json"),
        ),
        (
            "card_action_always.json",
            include_str!("../../tests/fixtures/lark/card_action_always.json"),
        ),
    ];
    for (name, raw_text) in fixtures {
        let raw: serde_json::Value =
            serde_json::from_str(raw_text).unwrap_or_else(|e| panic!("parse fixture {name}: {e}"));
        let sanitized = sanitize_card_action_payload(&raw);
        let dumped = serde_json::to_string(&sanitized).expect("sanitized fixture must serialize");
        for placeholder_field in [
            "REDACTED_TOKEN",
            "REDACTED_OPERATOR_OPEN_ID",
            "REDACTED_OPEN_CHAT_ID",
        ] {
            assert!(
                dumped.contains(placeholder_field),
                "sanitizer output for {name} must contain {placeholder_field}; got {dumped}"
            );
        }
    }
}

#[tokio::test]
async fn handle_card_action_event_routes_committed_fixtures() {
    use zeroclaw_api::channel::ChannelApprovalResponse;

    let fixtures = [
        (
            "approve",
            include_str!("../../tests/fixtures/lark/card_action_approve.json"),
            ChannelApprovalResponse::Approve,
        ),
        (
            "deny",
            include_str!("../../tests/fixtures/lark/card_action_deny.json"),
            ChannelApprovalResponse::Deny,
        ),
        (
            "always",
            include_str!("../../tests/fixtures/lark/card_action_always.json"),
            ChannelApprovalResponse::AlwaysApprove,
        ),
    ];

    for (name, raw, expected) in fixtures {
        let ch = make_channel();
        let event: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("parse {name} fixture: {e}"));
        let approval_id = event
            .pointer("/action/value/approval_id")
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("{name} fixture must contain an approval id"));
        assert!(
            !approval_id.is_empty(),
            "{name} approval id must be non-empty"
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        ch.pending_approvals.lock().await.insert(
            approval_id.to_string(),
            PendingApproval {
                sender: tx,
                message_id: String::new(),
                tool_name: String::new(),
                arguments_summary: String::new(),
            },
        );

        ch.handle_card_action_event(&event)
            .await
            .unwrap_or_else(|e| panic!("route {name} fixture: {e}"));
        let result = rx
            .await
            .unwrap_or_else(|e| panic!("receive {name} decision: {e}"));
        assert_eq!(result, expected, "fixture {name}");
    }
}

#[tokio::test]
async fn handle_card_action_event_parses_card_v2_behaviors_value_payload() {
    use zeroclaw_api::channel::ChannelApprovalResponse;

    // Card 2.0 button click events MAY round-trip via
    // event.action.behaviors[0].value instead of event.action.value.
    // Verify the dual-pointer fallback.
    let ch = make_channel();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let approval_id = "test-v2-approval".to_string();
    ch.pending_approvals.lock().await.insert(
        approval_id.clone(),
        PendingApproval {
            sender: tx,
            message_id: String::new(),
            tool_name: String::new(),
            arguments_summary: String::new(),
        },
    );

    let event = serde_json::json!({
        "action": {
            "tag": "button",
            "behaviors": [{
                "type": "callback",
                "value": { "approval_id": approval_id, "decision": "always" }
            }]
        }
    });
    ch.handle_card_action_event(&event)
        .await
        .expect("handler ok");
    let result = rx.await.expect("oneshot delivered");
    assert_eq!(result, ChannelApprovalResponse::AlwaysApprove);
}

#[tokio::test]
async fn unknown_lark_decision_cannot_become_an_operator_denial() {
    // An unrecognized `decision` value used to be mapped to Deny and sent
    // through the pending-approval oneshot. `wait_for_decision` stamps
    // everything it receives from that oneshot as `ApprovalSource::Operator`,
    // so a malformed card action arrived at the gate as "Denied by user."
    // even though no operator decided anything.
    //
    // The approval must be left PENDING so it resolves through the timeout
    // path, which carries runtime provenance.
    let ch = make_channel();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let approval_id = "test-unknown-decision".to_string();
    ch.pending_approvals.lock().await.insert(
        approval_id.clone(),
        PendingApproval {
            sender: tx,
            message_id: String::new(),
            tool_name: String::new(),
            arguments_summary: String::new(),
        },
    );

    let event = serde_json::json!({
        "action": {
            "tag": "button",
            "value": { "approval_id": approval_id, "decision": "sudo-make-me-a-sandwich" }
        }
    });
    let outcome = ch.handle_card_action_event(&event).await;
    assert!(
        outcome.is_err(),
        "an unknown decision must be rejected, not silently accepted"
    );

    // Nothing was sent: the receiver is still empty and still open, so the
    // gate will see a runtime-sourced timeout rather than an operator Deny.
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "no decision may be delivered for an unrecognized card action"
    );
    assert!(
        ch.pending_approvals.lock().await.contains_key(&approval_id),
        "the approval must stay pending so it resolves with runtime provenance"
    );
}

#[tokio::test]
async fn handle_card_action_event_for_unknown_approval_is_not_an_error() {
    let ch = make_channel();
    let event = serde_json::json!({
        "action": {
            "value": { "approval_id": "never-existed", "decision": "deny" }
        }
    });
    // Unknown approval IDs are dropped silently (info-log only); the
    // handler must NOT propagate an error to the caller, since stray
    // clicks (resent after restart) are routine.
    ch.handle_card_action_event(&event)
        .await
        .expect("unknown approval id should not error");
}
async fn mount_lark_token_and_send_mocks(mock_server: &wiremock::MockServer) {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("POST"))
        .and(path("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "test-tenant-token",
            "expire": 7200
        })))
        .mount(mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/im/v1/messages"))
        .and(query_param("receive_id_type", "chat_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "message_id": "om_test_message_id" }
        })))
        .expect(1)
        .mount(mock_server)
        .await;
}

async fn assert_send_body_matches_recipient_and_text(
    mock_server: &wiremock::MockServer,
    expected_recipient: &str,
    expected_text: &str,
) {
    let requests = mock_server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let send_request = requests
        .iter()
        .find(|r| r.url.path() == "/im/v1/messages")
        .expect("expected at least one POST /im/v1/messages");
    assert_eq!(
        send_request.url.query(),
        Some("receive_id_type=chat_id"),
        "send URL must carry receive_id_type=chat_id query param"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&send_request.body).expect("send body should be valid JSON");
    assert_eq!(
        body["receive_id"].as_str(),
        Some(expected_recipient),
        "receive_id must match the SendMessage recipient; full body: {body}"
    );
    assert_eq!(
        body["msg_type"].as_str(),
        Some("interactive"),
        "msg_type must be 'interactive'; full body: {body}"
    );
    let content_str = body["content"]
        .as_str()
        .expect("content must be a JSON string per Lark interactive-card spec");
    assert!(
        content_str.contains(expected_text),
        "card content should embed the message text {expected_text:?}; got: {content_str}"
    );
}

#[tokio::test]
async fn lark_send_via_from_config_emits_post_to_messages_endpoint() {
    let mock_server = wiremock::MockServer::start().await;
    mount_lark_token_and_send_mocks(&mock_server).await;

    let config = zeroclaw_config::schema::LarkConfig {
        enabled: true,
        use_feishu: false,
        app_id: "cli_test_app_id".to_string(),
        app_secret: "test_app_secret".to_string(),
        approval_timeout_secs: 300,
        ..Default::default()
    };
    let mut ch = LarkChannel::from_config(&config, "test_alias", resolver_from(vec![]));
    ch.api_base_override = Some(mock_server.uri());

    assert_eq!(
        ch.name(),
        "lark",
        "use_feishu=false must keep the channel identity as 'lark'"
    );

    let message = SendMessage::new("hi from cron", "oc_test_chat_id");
    Channel::send(&ch, &message)
        .await
        .expect("Channel::send should succeed against mocked Lark endpoint");

    assert_send_body_matches_recipient_and_text(&mock_server, "oc_test_chat_id", "hi from cron")
        .await;
}

#[tokio::test]
async fn feishu_send_via_from_config_emits_post_to_messages_endpoint() {
    let mock_server = wiremock::MockServer::start().await;
    mount_lark_token_and_send_mocks(&mock_server).await;

    let config = zeroclaw_config::schema::LarkConfig {
        enabled: true,
        use_feishu: true,
        app_id: "cli_test_app_id".to_string(),
        app_secret: "test_app_secret".to_string(),
        approval_timeout_secs: 300,
        ..Default::default()
    };
    let mut ch = LarkChannel::from_config(&config, "test_alias", resolver_from(vec![]));
    ch.api_base_override = Some(mock_server.uri());

    assert_eq!(
        ch.name(),
        "lark",
        "use_feishu=true still uses 'lark' as routing identity — \
             use_feishu only selects the API endpoint"
    );

    let message = SendMessage::new("hi from cron", "oc_test_chat_id");
    Channel::send(&ch, &message)
        .await
        .expect("Channel::send should succeed against mocked Feishu endpoint");

    assert_send_body_matches_recipient_and_text(&mock_server, "oc_test_chat_id", "hi from cron")
        .await;
}

#[tokio::test]
async fn lark_send_uploads_workspace_image_marker_after_text() {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    let mock_server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "test-tenant-token",
            "expire": 7200
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/im/v1/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "image_key": "img_test_key" }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/im/v1/messages"))
        .and(query_param("receive_id_type", "chat_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "message_id": "om_test_message_id" }
        })))
        .expect(2)
        .mount(&mock_server)
        .await;

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("photo.png"), b"\x89PNG\r\n\x1a\n").expect("write image");
    let config = zeroclaw_config::schema::LarkConfig {
        enabled: true,
        use_feishu: false,
        app_id: "cli_test_app_id".to_string(),
        app_secret: "test_app_secret".to_string(),
        approval_timeout_secs: 300,
        ..Default::default()
    };
    let mut ch = LarkChannel::from_config(&config, "test_alias", resolver_from(vec![]))
        .with_workspace_dir(workspace.path().to_path_buf());
    ch.api_base_override = Some(mock_server.uri());

    let message = SendMessage::new("caption [IMAGE:photo.png]", "oc_test_chat_id");
    Channel::send(&ch, &message)
        .await
        .expect("Channel::send should upload and send image marker");

    let requests = mock_server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let send_bodies = requests
        .iter()
        .filter(|request| request.url.path() == "/im/v1/messages")
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("send body should be valid JSON")
        })
        .collect::<Vec<_>>();

    assert!(
        send_bodies.iter().any(|body| {
            body["msg_type"].as_str() == Some("interactive")
                && body["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("caption"))
        }),
        "expected one interactive card send with caption; bodies: {send_bodies:?}"
    );
    let image_send = send_bodies
        .iter()
        .find(|body| body["msg_type"].as_str() == Some("image"))
        .expect("expected image send body");
    assert_eq!(image_send["receive_id"].as_str(), Some("oc_test_chat_id"));
    let content = image_send["content"]
        .as_str()
        .expect("image content should be a JSON string");
    let content_json: serde_json::Value =
        serde_json::from_str(content).expect("image content should parse as JSON");
    assert_eq!(content_json["image_key"].as_str(), Some("img_test_key"));
}

#[tokio::test]
async fn lark_send_uploads_workspace_document_marker_as_file_message() {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    let mock_server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "test-tenant-token",
            "expire": 7200
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/im/v1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "file_key": "file_test_key" }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/im/v1/messages"))
        .and(query_param("receive_id_type", "chat_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "message_id": "om_test_message_id" }
        })))
        .expect(2)
        .mount(&mock_server)
        .await;

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("brief.txt"), b"brief").expect("write document");
    let config = zeroclaw_config::schema::LarkConfig {
        enabled: true,
        use_feishu: false,
        app_id: "cli_test_app_id".to_string(),
        app_secret: "test_app_secret".to_string(),
        approval_timeout_secs: 300,
        ..Default::default()
    };
    let mut ch = LarkChannel::from_config(&config, "test_alias", resolver_from(vec![]))
        .with_workspace_dir(workspace.path().to_path_buf());
    ch.api_base_override = Some(mock_server.uri());

    let message = SendMessage::new("see attached [DOCUMENT:brief.txt]", "oc_test_chat_id");
    Channel::send(&ch, &message)
        .await
        .expect("Channel::send should upload and send document marker");

    let requests = mock_server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let send_bodies = requests
        .iter()
        .filter(|request| request.url.path() == "/im/v1/messages")
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("send body should be valid JSON")
        })
        .collect::<Vec<_>>();

    let file_send = send_bodies
        .iter()
        .find(|body| body["msg_type"].as_str() == Some("file"))
        .expect("expected file send body");
    assert_eq!(file_send["receive_id"].as_str(), Some("oc_test_chat_id"));
    let content = file_send["content"]
        .as_str()
        .expect("file content should be a JSON string");
    let content_json: serde_json::Value =
        serde_json::from_str(content).expect("file content should parse as JSON");
    assert_eq!(content_json["file_key"].as_str(), Some("file_test_key"));
}

#[tokio::test]
async fn lark_finalize_draft_cleans_marker_text_and_sends_media() {
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, ResponseTemplate};

    let mock_server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "test-tenant-token",
            "expire": 7200
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/im/v1/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "image_key": "draft_img_key" }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex("/im/v1/messages/om_draft_media"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 0 })))
        .expect(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/im/v1/messages"))
        .and(query_param("receive_id_type", "chat_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": { "message_id": "om_test_message_id" }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("draft.png"), b"\x89PNG\r\n\x1a\n").expect("write image");
    let mut ch = make_channel()
        .with_streaming(StreamMode::Partial, 500)
        .with_workspace_dir(workspace.path().to_path_buf());
    ch.api_base_override = Some(mock_server.uri());

    ch.finalize_draft(
        "oc_test_chat_id",
        "om_draft_media",
        "final caption [IMAGE:draft.png]",
        false,
    )
    .await
    .expect("finalize_draft should clean text and send image");

    let requests = mock_server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let patch = requests
        .iter()
        .find(|request| request.method.as_str() == "PATCH")
        .expect("expected draft PATCH");
    let patch_body = String::from_utf8_lossy(&patch.body);
    assert!(patch_body.contains("final caption"));
    assert!(
        !patch_body.contains("[IMAGE:"),
        "final draft body must not leak marker text: {patch_body}"
    );
    let image_send = requests
        .iter()
        .filter(|request| request.url.path() == "/im/v1/messages")
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("send body should be valid JSON")
        })
        .find(|body| body["msg_type"].as_str() == Some("image"))
        .expect("expected image send after draft finalization");
    let content = image_send["content"]
        .as_str()
        .expect("image content should be a JSON string");
    let content_json: serde_json::Value =
        serde_json::from_str(content).expect("image content should parse as JSON");
    assert_eq!(content_json["image_key"].as_str(), Some("draft_img_key"));
}

#[test]
fn unicode_to_lark_emoji_type_covers_known_noreply_emojis() {
    assert_eq!(unicode_to_lark_emoji_type("👍"), Some("THUMBSUP"));
    assert_eq!(unicode_to_lark_emoji_type("🚫"), Some("No"));
    assert_eq!(unicode_to_lark_emoji_type("⚠️"), Some("Alarm"));
    assert_eq!(unicode_to_lark_emoji_type("👀"), Some("GLANCE"));
    assert_eq!(unicode_to_lark_emoji_type("✅"), Some("DONE"));
    assert_eq!(unicode_to_lark_emoji_type("🎉"), Some("PARTY"));
    assert_eq!(unicode_to_lark_emoji_type("🙉"), None);
    assert_ne!(unicode_to_lark_emoji_type("🚫"), Some("NO"));
}

#[tokio::test]
async fn lark_inbound_channel_message_id_is_om_xxx_not_uuid() {
    let ch = make_channel();
    let om_id = "om_ack_reaction_compat_xyz";
    let payload = serde_json::json!({
        "header": {
            "event_type": "im.message.receive_v1"
        },
        "event": {
            "sender": {
                "sender_id": {
                    "open_id": "ou_testuser123"
                }
            },
            "message": {
                "message_id": om_id,
                "message_type": "text",
                "content": "{\"text\":\"ack test\"}",
                "chat_id": "oc_chat123",
                "chat_type": "p2p",
                "create_time": "1699999999000"
            }
        }
    });

    let msgs = ch.parse_event_payload_async(&payload).await;
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0].id, om_id,
        "ChannelMessage.id must equal the Feishu om_xxx message_id; \
             otherwise add_reaction returns 99992354 (id not exist). \
             Got: {:?}",
        msgs[0].id
    );

    // Belt-and-suspenders: explicitly assert msg.id is NOT a
    // UUID-v4 shape (8-4-4-4-12 hex with hyphens). Future "let's
    // just use UUID" PRs will fail this and prompt a re-read.
    fn looks_like_uuid_v4(s: &str) -> bool {
        let bytes = s.as_bytes();
        if bytes.len() != 36 {
            return false;
        }
        for (i, &b) in bytes.iter().enumerate() {
            let is_hyphen_pos = i == 8 || i == 13 || i == 18 || i == 23;
            if is_hyphen_pos {
                if b != b'-' {
                    return false;
                }
            } else if !b.is_ascii_hexdigit() {
                return false;
            }
        }
        true
    }
    assert!(
        !looks_like_uuid_v4(&msgs[0].id),
        "ChannelMessage.id must NOT be a UUID-v4 shape — Feishu \
             add_reaction requires the native om_xxx open_message_id. \
             Got: {:?}",
        msgs[0].id
    );
}

#[tokio::test]
async fn remove_reaction_caches_id_from_add_and_deletes() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_api::channel::Channel;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-rm-ok",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    let post_mock = Mock::given(method("POST"))
        .and(path_regex("/im/v1/messages/om_test/reactions$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "reaction_id": "r_xyz",
                "operator": { "operator_id": "cli_test", "operator_type": "app" },
                "action_time": "1700000000000",
                "reaction_type": { "emoji_type": "GLANCE" }
            }
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let delete_mock = Mock::given(method("DELETE"))
        .and(path_regex("/im/v1/messages/om_test/reactions/r_xyz$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 0 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(server.uri());

    ch.add_reaction("oc_chat", "om_test", "\u{1F440}")
        .await
        .expect("add_reaction should succeed");
    ch.remove_reaction("oc_chat", "om_test", "\u{1F440}")
        .await
        .expect("remove_reaction should succeed");

    let cache = ch.reaction_ids.lock().await;
    assert!(
        cache.is_empty(),
        "reaction_ids cache should be empty after remove, got {} entries",
        cache.len()
    );

    drop(post_mock);
    drop(delete_mock);
}

#[tokio::test]
async fn remove_reaction_silent_on_cache_miss() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_api::channel::Channel;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-rm-miss",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    let delete_mock = Mock::given(method("DELETE"))
        .and(path_regex("/im/v1/messages/.*/reactions/.*"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(server.uri());

    ch.remove_reaction("oc_chat", "om_never_added", "\u{1F440}")
        .await
        .expect("cache miss must not error");

    drop(delete_mock);
}

#[tokio::test]
async fn remove_reaction_tolerates_server_stale_codes() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_api::channel::Channel;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-rm-stale",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex("/im/v1/messages/om_stale/reactions$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "reaction_id": "r_stale",
                "operator": { "operator_id": "cli_test", "operator_type": "app" },
                "action_time": "1700000000000",
                "reaction_type": { "emoji_type": "GLANCE" }
            }
        })))
        .mount(&server)
        .await;

    let delete_mock = Mock::given(method("DELETE"))
        .and(path_regex("/im/v1/messages/om_stale/reactions/r_stale$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 231_007,
            "msg": "operator has no permission to delete this reaction"
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(server.uri());

    ch.add_reaction("oc_chat", "om_stale", "\u{1F440}")
        .await
        .expect("add_reaction should succeed");
    ch.remove_reaction("oc_chat", "om_stale", "\u{1F440}")
        .await
        .expect("stale-state code must not propagate as error");

    let cache = ch.reaction_ids.lock().await;
    assert!(
        cache.is_empty(),
        "reaction_ids cache should be empty after stale-state DELETE"
    );

    drop(delete_mock);
}

#[tokio::test]
async fn add_reaction_caches_glance_under_unicode_key() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_api::channel::Channel;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-glance",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    let post_mock = Mock::given(method("POST"))
        .and(path_regex("/im/v1/messages/om_glance/reactions$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "reaction_id": "r_glance_xyz",
                "operator": { "operator_id": "cli_test", "operator_type": "app" },
                "action_time": "1700000000000",
                "reaction_type": { "emoji_type": "GLANCE" }
            }
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(server.uri());

    ch.add_reaction("oc_chat", "om_glance", "\u{1F440}")
        .await
        .expect("add_reaction should succeed");

    let cache = ch.reaction_ids.lock().await;
    let stored = cache
        .get(&("om_glance".to_string(), "\u{1F440}".to_string()))
        .cloned();
    assert_eq!(
        stored.as_deref(),
        Some("r_glance_xyz"),
        "reaction_id must be cached under unicode 👀 key, got {stored:?}"
    );
    assert!(
        cache
            .get(&("om_glance".to_string(), "GLANCE".to_string()))
            .is_none(),
        "reaction_id must NOT be cached under Feishu emoji_type 'GLANCE'"
    );

    drop(post_mock);
}

#[tokio::test]
async fn lark_inbound_ack_lifecycle_swaps_glance_to_done_with_no_orphan() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_api::channel::Channel;

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-lifecycle",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    // POST 👀 (GLANCE) — must be invoked EXACTLY once.
    // If a regression re-adds a Lark-local fast-ack spawn alongside
    // the generic orchestrator add_reaction call, this mock would see
    // a second POST and the assertion below would fail.
    let post_glance_mock = Mock::given(method("POST"))
        .and(path_regex("/im/v1/messages/om_lifecycle/reactions$"))
        .and(wiremock::matchers::body_string_contains("GLANCE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "reaction_id": "r_glance_lifecycle",
                "operator": { "operator_id": "cli_test", "operator_type": "app" },
                "action_time": "1700000000000",
                "reaction_type": { "emoji_type": "GLANCE" }
            }
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    // DELETE on the cached GLANCE reaction_id — must be invoked
    // EXACTLY once. Cache-miss path would silently skip the DELETE
    // (see `remove_reaction` doc) and this expect(1) would fail.
    let delete_glance_mock = Mock::given(method("DELETE"))
        .and(path_regex(
            "/im/v1/messages/om_lifecycle/reactions/r_glance_lifecycle$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 0 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    // POST ✅ (DONE) — must be invoked EXACTLY once.
    let post_done_mock = Mock::given(method("POST"))
        .and(path_regex("/im/v1/messages/om_lifecycle/reactions$"))
        .and(wiremock::matchers::body_string_contains("DONE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "reaction_id": "r_done_lifecycle",
                "operator": { "operator_id": "cli_test", "operator_type": "app" },
                "action_time": "1700000000001",
                "reaction_type": { "emoji_type": "DONE" }
            }
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(server.uri());

    // Drive the lifecycle through the public Channel trait — the
    // same surface the generic orchestrator uses in production.
    ch.add_reaction("oc_chat", "om_lifecycle", "\u{1F440}")
        .await
        .expect("add 👀 should succeed");
    ch.remove_reaction("oc_chat", "om_lifecycle", "\u{1F440}")
        .await
        .expect("remove 👀 should succeed");
    ch.add_reaction("oc_chat", "om_lifecycle", "\u{2705}")
        .await
        .expect("add ✅ should succeed");

    // Cache shape: ✅ present, 👀 gone, no orphans.
    let cache = ch.reaction_ids.lock().await;
    assert_eq!(
        cache.len(),
        1,
        "after lifecycle the cache must contain exactly 1 entry (✅), got {}: {:?}",
        cache.len(),
        cache.keys().collect::<Vec<_>>()
    );
    assert!(
        cache
            .get(&("om_lifecycle".to_string(), "\u{1F440}".to_string()))
            .is_none(),
        "the 👀 entry must be gone after remove_reaction; \
             orphan presence indicates a parallel ack path bypassed the cache"
    );
    assert_eq!(
        cache
            .get(&("om_lifecycle".to_string(), "\u{2705}".to_string()))
            .map(String::as_str),
        Some("r_done_lifecycle"),
        "✅ reaction_id must be cached under its unicode key"
    );

    // Mock-scope drop verifies the .expect(N) counts. A regression
    // that POSTs 👀 twice (fast-ack + generic) makes post_glance_mock
    // fail with 'received 2 requests, expected 1'.
    drop(post_glance_mock);
    drop(delete_glance_mock);
    drop(post_done_mock);
}

#[tokio::test]
async fn lark_fast_ack_and_generic_path_dedupe_on_cache_hit() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_api::channel::Channel;

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "tenant_access_token": "t-dedupe",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    let post_glance_mock = Mock::given(method("POST"))
        .and(path_regex("/im/v1/messages/om_dedupe/reactions$"))
        .and(wiremock::matchers::body_string_contains("GLANCE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {
                "reaction_id": "r_dedupe_fast_ack",
                "operator": { "operator_id": "cli_test", "operator_type": "app" },
                "action_time": "1700000000000",
                "reaction_type": { "emoji_type": "GLANCE" }
            }
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    // DELETE on the cached reaction_id from the FAST-ACK POST — proves
    // that fast-ack's reaction_id survived through the dedupe path
    // and is still usable for cleanup.
    let delete_glance_mock = Mock::given(method("DELETE"))
        .and(path_regex(
            "/im/v1/messages/om_dedupe/reactions/r_dedupe_fast_ack$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 0 })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let mut ch = make_channel();
    ch.api_base_override = Some(server.uri());

    // Step 1: fast-ack POSTs 👀 and writes (om_dedupe, "👀") → R1.
    ch.add_reaction("oc_chat", "om_dedupe", "\u{1F440}")
        .await
        .expect("fast-ack add 👀 should succeed");

    // Sanity: cache populated.
    {
        let cache = ch.reaction_ids.lock().await;
        assert_eq!(
            cache
                .get(&("om_dedupe".to_string(), "\u{1F440}".to_string()))
                .map(String::as_str),
            Some("r_dedupe_fast_ack"),
            "fast-ack must populate cache under unicode 👀 key"
        );
    }

    ch.add_reaction("oc_chat", "om_dedupe", "\u{1F440}")
        .await
        .expect("generic-path add 👀 must be cache-hit no-op, not error");

    // Cache must still hold the SAME reaction_id from the fast-ack —
    // the dedupe path must not overwrite it.
    {
        let cache = ch.reaction_ids.lock().await;
        assert_eq!(
            cache
                .get(&("om_dedupe".to_string(), "\u{1F440}".to_string()))
                .map(String::as_str),
            Some("r_dedupe_fast_ack"),
            "cache value must remain the fast-ack reaction_id after dedupe \
                 (no overwrite)"
        );
    }

    ch.remove_reaction("oc_chat", "om_dedupe", "\u{1F440}")
        .await
        .expect("remove 👀 should DELETE the fast-ack reaction_id");

    // Cache must be empty after remove.
    {
        let cache = ch.reaction_ids.lock().await;
        assert!(
            cache.is_empty(),
            "cache must be empty after remove_reaction, got {} entries",
            cache.len()
        );
    }

    drop(post_glance_mock);
    drop(delete_glance_mock);
}
