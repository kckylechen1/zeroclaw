mod markers {
    use super::super::markers::{MarkerKind, parse};

    #[test]
    fn empty_text_yields_no_markers() {
        let (text, ms) = parse("");
        assert_eq!(text, "");
        assert!(ms.is_empty());
    }

    #[test]
    fn plain_text_passthrough() {
        let (text, ms) = parse("hello world");
        assert_eq!(text, "hello world");
        assert!(ms.is_empty());
    }

    #[test]
    fn single_image_marker_extracted() {
        let (text, ms) = parse("[image:https://example.com/cat.jpg]");
        assert_eq!(text, "");
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, MarkerKind::Image);
        assert_eq!(ms[0].target, "https://example.com/cat.jpg");
    }

    #[test]
    fn voice_marker_distinct_from_audio() {
        let (_, ms) = parse("[voice:/tmp/note.ogg] [audio:/tmp/song.mp3]");
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].kind, MarkerKind::Voice);
        assert_eq!(ms[1].kind, MarkerKind::Audio);
    }

    #[test]
    fn multiple_markers_with_text_in_between() {
        let (text, ms) = parse("before [image:https://x/y.jpg] middle [file:/tmp/doc.pdf] after");
        assert_eq!(text, "before  middle  after");
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].kind, MarkerKind::Image);
        assert_eq!(ms[1].kind, MarkerKind::File);
    }

    #[test]
    fn malformed_marker_left_in_text() {
        let (text, ms) = parse("foo [image: bar");
        assert_eq!(text, "foo [image: bar");
        assert!(ms.is_empty());
    }

    #[test]
    fn unknown_keyword_left_in_text() {
        let (text, ms) = parse("[banana:fruit]");
        assert_eq!(text, "[banana:fruit]");
        assert!(ms.is_empty());
    }

    #[test]
    fn empty_target_left_in_text() {
        let (text, ms) = parse("[image:]");
        assert_eq!(text, "[image:]");
        assert!(ms.is_empty());
    }

    #[test]
    fn marker_with_newline_inside_left_in_text() {
        let (text, ms) = parse("[image:a\nb]");
        assert!(text.contains("[image:a"));
        assert!(ms.is_empty());
    }
}

mod approval {
    use super::super::approval::{TOKEN_LEN, generate_token, generate_token_default, parse_reply};
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::collections::HashSet;
    use zeroclaw_api::channel::ChannelApprovalResponse;

    #[test]
    fn token_length_and_alphabet() {
        let mut rng = StdRng::seed_from_u64(42);
        let tok = generate_token(&mut rng);
        assert_eq!(tok.len(), TOKEN_LEN);
        assert!(tok.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn tokens_are_diverse() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            seen.insert(generate_token(&mut rng));
        }
        assert!(
            seen.len() >= 998,
            "too many collisions: {}",
            1000 - seen.len()
        );
    }

    #[test]
    fn default_token_has_correct_length() {
        assert_eq!(generate_token_default().len(), TOKEN_LEN);
    }

    #[test]
    fn parse_approve() {
        let (tok, resp) = parse_reply("ABCDEFGH approve").expect("parses");
        assert_eq!(tok, "ABCDEFGH");
        assert_eq!(resp, ChannelApprovalResponse::Approve);
    }

    #[test]
    fn parse_deny_lowercase() {
        let (_, resp) = parse_reply("abcdefgh deny").expect("parses");
        assert_eq!(resp, ChannelApprovalResponse::Deny);
    }

    #[test]
    fn parse_always() {
        let (_, resp) = parse_reply("ABCDEFGH always").expect("parses");
        assert_eq!(resp, ChannelApprovalResponse::AlwaysApprove);
    }

    #[test]
    fn parse_yes_no_aliases() {
        assert_eq!(
            parse_reply("ABCDEFGH yes").map(|x| x.1),
            Some(ChannelApprovalResponse::Approve)
        );
        assert_eq!(
            parse_reply("ABCDEFGH no").map(|x| x.1),
            Some(ChannelApprovalResponse::Deny)
        );
    }

    #[test]
    fn rejects_wrong_token_length() {
        assert!(parse_reply("ABC approve").is_none());
        assert!(parse_reply("ABCDEFGHIJ approve").is_none());
    }

    #[test]
    fn rejects_unknown_verb() {
        assert!(parse_reply("ABCDEFGH maybe").is_none());
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse_reply("ABCDEFGH approve please").is_none());
    }
}

mod room_management {
    use super::super::room_management::{build_create_room_request, build_invite_user_request};
    use matrix_sdk::ruma::api::client::room::Visibility as MatrixVisibility;
    use serde_json::json;
    use zeroclaw_api::channel::{RoomCreationOptions, RoomVisibility};

    #[test]
    fn create_room_request_maps_typed_options() {
        let request = build_create_room_request(&RoomCreationOptions {
            name: Some("Ops room".into()),
            topic: Some("Operations".into()),
            invites: vec!["@alice:example.org".into(), "@bob:example.org".into()],
            visibility: Some(RoomVisibility::Public),
            encryption: Some(true),
        })
        .expect("request builds");

        assert_eq!(request.name.as_deref(), Some("Ops room"));
        assert_eq!(request.topic.as_deref(), Some("Operations"));
        assert_eq!(request.visibility, MatrixVisibility::Public);
        assert_eq!(request.invite.len(), 2);
        assert_eq!(request.invite[0].as_str(), "@alice:example.org");
        assert_eq!(request.invite[1].as_str(), "@bob:example.org");
        assert_eq!(request.initial_state.len(), 1);
    }

    #[test]
    fn create_room_request_rejects_invalid_invite_user() {
        let err = build_create_room_request(&RoomCreationOptions {
            invites: vec!["not-a-mxid".into()],
            ..RoomCreationOptions::default()
        })
        .unwrap_err();

        assert!(err.to_string().contains("invalid invite user id"));
    }

    #[test]
    fn invite_user_request_parses_room_and_user_ids() {
        let request = build_invite_user_request("!room:example.org", "@alice:example.org").unwrap();

        assert_eq!(request.room_id.as_str(), "!room:example.org");
        assert_eq!(
            serde_json::to_value(&request.recipient).unwrap(),
            json!({"user_id": "@alice:example.org"})
        );
    }

    #[test]
    fn invite_user_request_rejects_invalid_ids() {
        let err = build_invite_user_request("not-a-room", "@alice:example.org").unwrap_err();
        assert!(err.to_string().contains("invalid room id"));

        let err = build_invite_user_request("!room:example.org", "not-a-user").unwrap_err();
        assert!(err.to_string().contains("invalid user id"));
    }
}

mod mention {
    use super::super::mention::is_mentioned;
    use matrix_sdk::ruma::user_id;

    #[test]
    fn explicit_mention_in_user_ids_passes() {
        let bot = user_id!("@bot:example.org");
        assert!(is_mentioned(
            bot,
            None,
            Some(&["@bot:example.org".to_string()]),
            "hi",
        ));
    }

    #[test]
    fn explicit_mention_list_without_bot_rejects() {
        let bot = user_id!("@bot:example.org");
        assert!(!is_mentioned(
            bot,
            None,
            Some(&["@alice:example.org".to_string()]),
            "@bot:example.org help",
        ));
    }

    #[test]
    fn body_fallback_full_id() {
        let bot = user_id!("@bot:example.org");
        assert!(is_mentioned(bot, None, None, "@bot:example.org help"));
    }

    #[test]
    fn body_fallback_localpart_only() {
        let bot = user_id!("@bot:example.org");
        assert!(is_mentioned(bot, None, None, "hey @bot please reply"));
    }

    #[test]
    fn body_fallback_display_name() {
        let bot = user_id!("@bot:example.org");
        assert!(is_mentioned(bot, Some("ZeroClaw"), None, "hi zeroclaw!"));
    }

    #[test]
    fn no_mention_rejects() {
        let bot = user_id!("@bot:example.org");
        assert!(!is_mentioned(
            bot,
            Some("ZeroClaw"),
            None,
            "no mention here"
        ));
    }
}

mod allowlist {
    use super::super::allowlist::{room_allowed_static, user_allowed};

    #[test]
    fn empty_user_list_denies_all() {
        assert!(!user_allowed(&[], "@a:b"));
    }

    #[test]
    fn star_user_list_allows_all() {
        assert!(user_allowed(&["*".to_string()], "@a:b"));
    }

    #[test]
    fn user_in_list_allowed() {
        assert!(user_allowed(&["@a:b".to_string()], "@a:b"));
    }

    #[test]
    fn user_not_in_list_denied() {
        assert!(!user_allowed(&["@a:b".to_string()], "@c:d"));
    }

    #[test]
    fn user_in_list_case_insensitive() {
        // Operator-configured case shouldn't matter — Matrix MXIDs are
        // spec-lowercase but tolerated in mixed case by some servers.
        assert!(user_allowed(
            &["@Bot:Example.org".to_string()],
            "@bot:example.org"
        ));
        assert!(user_allowed(
            &["@bot:example.org".to_string()],
            "@Bot:EXAMPLE.org"
        ));
    }

    #[test]
    fn empty_room_list_allows_all() {
        assert!(room_allowed_static(&[], "!any:server"));
    }

    #[test]
    fn room_in_list_allowed() {
        assert!(room_allowed_static(
            &["!ok:server".to_string()],
            "!ok:server"
        ));
    }

    #[test]
    fn room_not_in_list_denied() {
        assert!(!room_allowed_static(
            &["!ok:server".to_string()],
            "!nope:server"
        ));
    }
}

mod ack_reactions {
    use std::sync::Arc;

    use tempfile::TempDir;
    use zeroclaw_api::channel::Channel;
    use zeroclaw_config::schema::MatrixConfig;

    use super::super::MatrixChannel;

    #[tokio::test]
    async fn matrix_remove_reaction_noops_before_parsing_when_ack_disabled() {
        let config = MatrixConfig {
            homeserver: "https://matrix.example.com".to_string(),
            access_token: Some("token".to_string()),
            ack_reactions: Some(false),
            ..MatrixConfig::default()
        };
        let state_dir = TempDir::new().expect("temp state dir");
        let channel = MatrixChannel::new(
            config,
            "matrix",
            Arc::new(Vec::<String>::new),
            state_dir.path().to_path_buf(),
        )
        .expect("matrix channel");

        channel
            .remove_reaction("bad-room", "bad-event", "✅")
            .await
            .expect("ack-disabled reaction removal should be a no-op");
    }
}

mod context {
    use super::super::context::{claim_first_visit, format_preamble, mark_seen};
    use matrix_sdk::ruma::{OwnedEventId, owned_event_id};
    use std::{collections::HashSet, sync::Arc};
    use tokio::sync::RwLock;

    fn empty() -> Arc<RwLock<HashSet<OwnedEventId>>> {
        Arc::new(RwLock::new(HashSet::new()))
    }

    #[test]
    fn preamble_includes_sender_and_body() {
        let p = format_preamble("@alice:server", "hello");
        assert_eq!(p, "[Thread root from @alice:server]: hello\n\n");
    }

    #[test]
    fn preamble_skips_body_when_empty() {
        let p = format_preamble("@alice:server", "");
        assert_eq!(p, "[Thread root from @alice:server]\n\n");
    }

    #[tokio::test]
    async fn first_visit_returns_true_then_false() {
        let set = empty();
        let id = owned_event_id!("$abc:server");
        assert!(claim_first_visit(&set, &id).await);
        assert!(!claim_first_visit(&set, &id).await);
    }

    #[tokio::test]
    async fn pre_marked_thread_returns_false() {
        let set = empty();
        let id = owned_event_id!("$abc:server");
        mark_seen(&set, id.clone()).await;
        assert!(!claim_first_visit(&set, &id).await);
    }
}

mod streaming {
    use super::super::streaming;
    use super::super::streaming::{
        MultiDraft, PartialDraft, PartialFinalizeAction, State, decide_partial_finalize_action,
        partial_should_edit, partial_visible_text,
    };
    use matrix_sdk::ruma::{OwnedEventId, owned_event_id, owned_room_id};
    use std::time::{Duration, Instant};

    fn draft(text: &str, last_edit: Instant) -> PartialDraft {
        PartialDraft {
            event_id: owned_event_id!("$1:server"),
            thread_anchor: None,
            last_text: text.to_string(),
            last_edit,
        }
    }

    fn partial_draft(event_id: OwnedEventId, text: &str) -> PartialDraft {
        PartialDraft {
            event_id,
            thread_anchor: None,
            last_text: text.to_string(),
            last_edit: Instant::now(),
        }
    }

    #[test]
    fn skip_when_text_unchanged() {
        let now = Instant::now();
        let d = draft("hello", now - Duration::from_secs(60));
        assert!(!partial_should_edit(
            &d,
            "hello",
            now,
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn skip_within_rate_limit() {
        let now = Instant::now();
        let d = draft("hello", now - Duration::from_millis(100));
        assert!(!partial_should_edit(
            &d,
            "world",
            now,
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn allow_after_rate_limit() {
        let now = Instant::now();
        let d = draft("hello", now - Duration::from_millis(600));
        assert!(partial_should_edit(
            &d,
            "world",
            now,
            Duration::from_millis(500)
        ));
    }

    #[test]
    fn partial_visible_text_strips_attachment_markers() {
        assert_eq!(
            partial_visible_text("Report ready [DOCUMENT:report.pdf]").as_deref(),
            Some("Report ready")
        );
    }

    #[test]
    fn partial_visible_text_skips_marker_only_updates() {
        assert_eq!(partial_visible_text("[DOCUMENT:report.pdf]"), None);
    }

    #[test]
    fn marker_only_partial_finalize_redacts_placeholder_after_upload() {
        assert_eq!(
            decide_partial_finalize_action(true, true),
            PartialFinalizeAction::RedactDraft
        );
    }

    #[test]
    fn text_partial_finalize_keeps_editing_draft_after_upload() {
        assert_eq!(
            decide_partial_finalize_action(false, true),
            PartialFinalizeAction::EditDraft
        );
    }

    #[test]
    fn text_only_partial_finalize_keeps_editing_draft() {
        assert_eq!(
            decide_partial_finalize_action(false, false),
            PartialFinalizeAction::EditDraft
        );
    }

    #[test]
    fn empty_partial_finalize_without_upload_reports_empty_error() {
        assert_eq!(
            decide_partial_finalize_action(true, false),
            PartialFinalizeAction::EmptyError
        );
    }

    #[test]
    fn draft_keys_include_message_id_for_same_room_concurrency() {
        let room = owned_room_id!("!room:server");
        let first = streaming::draft_key(room.clone(), "$draft-a:server").unwrap();
        let second = streaming::draft_key(room.clone(), "$draft-b:server").unwrap();

        assert_ne!(first, second);

        let mut state = streaming::State::default();
        state.partial.insert(
            first.clone(),
            PartialDraft {
                event_id: owned_event_id!("$draft-a:server"),
                thread_anchor: None,
                last_text: "first".to_string(),
                last_edit: Instant::now(),
            },
        );
        state.partial.insert(
            second.clone(),
            PartialDraft {
                event_id: owned_event_id!("$draft-b:server"),
                thread_anchor: None,
                last_text: "second".to_string(),
                last_edit: Instant::now(),
            },
        );

        assert_eq!(state.partial.len(), 2);
        assert_eq!(
            state.partial.remove(&second).map(|draft| draft.event_id),
            Some(owned_event_id!("$draft-b:server"))
        );
        assert!(state.partial.contains_key(&first));
    }

    #[test]
    fn partial_lifecycle_lookup_isolates_update_finalize_and_cancel_by_message_id() {
        let recipient = "!room:server";
        let first = super::super::streaming_key(recipient, "$draft-a:server").unwrap();
        let second = super::super::streaming_key(recipient, "$draft-b:server").unwrap();
        let canceled = super::super::streaming_key(recipient, "$draft-c:server").unwrap();

        let mut state = State::default();
        state.partial.insert(
            first.clone(),
            partial_draft(owned_event_id!("$draft-a:server"), "first"),
        );
        state.partial.insert(
            second.clone(),
            partial_draft(owned_event_id!("$draft-b:server"), "second"),
        );

        streaming::partial_for_update(&mut state, &second)
            .expect("second draft remains addressable")
            .last_text = "second updated".to_string();

        assert_eq!(
            streaming::partial_for_update(&mut state, &first)
                .expect("first draft remains isolated")
                .last_text,
            "first"
        );

        let finalized = streaming::take_partial(&mut state, &second)
            .expect("finalize removes only the addressed draft");
        assert_eq!(finalized.event_id, owned_event_id!("$draft-b:server"));
        assert!(state.partial.contains_key(&first));
        assert!(!state.partial.contains_key(&second));

        state.partial.insert(
            canceled.clone(),
            partial_draft(owned_event_id!("$draft-c:server"), "cancel me"),
        );
        let canceled_draft = streaming::take_partial(&mut state, &canceled)
            .expect("cancel removes only the addressed draft");
        assert_eq!(canceled_draft.event_id, owned_event_id!("$draft-c:server"));
        assert!(state.partial.contains_key(&first));
        assert!(!state.partial.contains_key(&canceled));
    }

    #[test]
    fn multi_message_lifecycle_lookup_isolates_update_finalize_and_cancel_by_message_id() {
        let recipient = "!room:server";
        let first =
            super::super::streaming_key(recipient, "multi_message_synthetic:first").unwrap();
        let second =
            super::super::streaming_key(recipient, "multi_message_synthetic:second").unwrap();
        let canceled =
            super::super::streaming_key(recipient, "multi_message_synthetic:cancel").unwrap();

        let mut state = State::default();
        state.multi.insert(
            first.clone(),
            MultiDraft {
                thread_anchor: None,
                sent_so_far: 5,
            },
        );
        state.multi.insert(
            second.clone(),
            MultiDraft {
                thread_anchor: None,
                sent_so_far: 0,
            },
        );

        streaming::multi_for_update(&mut state, &second)
            .expect("second multi-message draft remains addressable")
            .sent_so_far = 12;

        assert_eq!(
            streaming::multi_for_update(&mut state, &first)
                .expect("first multi-message draft remains isolated")
                .sent_so_far,
            5
        );

        let finalized = streaming::take_multi(&mut state, &second)
            .expect("finalize removes only the addressed multi-message draft");
        assert_eq!(finalized.sent_so_far, 12);
        assert!(state.multi.contains_key(&first));
        assert!(!state.multi.contains_key(&second));

        state.multi.insert(
            canceled.clone(),
            MultiDraft {
                thread_anchor: None,
                sent_so_far: 3,
            },
        );
        let canceled_draft = streaming::take_multi(&mut state, &canceled)
            .expect("cancel removes only the addressed multi-message draft");
        assert_eq!(canceled_draft.sent_so_far, 3);
        assert!(state.multi.contains_key(&first));
        assert!(!state.multi.contains_key(&canceled));
    }

    #[test]
    fn multi_message_synthetic_draft_ids_are_unique() {
        let first = streaming::new_multi_message_draft_id();
        let second = streaming::new_multi_message_draft_id();

        assert_ne!(first, second);
        assert!(first.starts_with("multi_message_synthetic:"));
        assert!(second.starts_with("multi_message_synthetic:"));
    }
}

mod live_smoke {
    use std::{
        env,
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use matrix_sdk::config::SyncSettings;
    use tempfile::TempDir;
    use zeroclaw_api::channel::{Channel, SendMessage};
    use zeroclaw_config::schema::{MatrixConfig, StreamMode};

    use super::super::{MatrixChannel, inbound::SYNC_LONGPOLL_TIMEOUT, streaming_key};

    fn env_first(primary: &str, fallback: &str) -> String {
        env::var(primary)
            .or_else(|_| env::var(fallback))
            .unwrap_or_else(|_| panic!("set {primary} or {fallback} to run Matrix live smoke"))
    }

    #[tokio::test]
    #[ignore = "requires Matrix smoke credentials and a disposable test room"]
    async fn same_room_partial_draft_lifecycle_uses_real_draft_ids() {
        let homeserver = env_first(
            "ZEROCLAW_MATRIX_SMOKE_HOMESERVER",
            "ZEROCLAW_MATRIX_HOMESERVER",
        );
        let room_id = env_first("ZEROCLAW_MATRIX_SMOKE_ROOM_ID", "ZEROCLAW_MATRIX_ROOM_ID");
        let access_token = env_first(
            "ZEROCLAW_MATRIX_SMOKE_ACCESS_TOKEN",
            "ZEROCLAW_MATRIX_ACCESS_TOKEN",
        );
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_secs();

        let config = MatrixConfig {
            enabled: true,
            homeserver,
            access_token: Some(access_token),
            allowed_rooms: vec![room_id.clone()],
            stream_mode: StreamMode::Partial,
            draft_update_interval_ms: 50,
            multi_message_delay_ms: 0,
            reply_in_thread: false,
            ack_reactions: Some(false),
            approval_timeout_secs: 1,
            ..MatrixConfig::default()
        };
        let state_dir = TempDir::new().expect("temp state dir");
        let channel = MatrixChannel::new(
            config,
            "matrix",
            Arc::new(Vec::<String>::new),
            state_dir.path().to_path_buf(),
        )
        .expect("matrix channel");

        let client = channel.ensure_client().await.expect("matrix client");
        client
            .sync_once(SyncSettings::default().timeout(SYNC_LONGPOLL_TIMEOUT))
            .await
            .expect("initial Matrix sync");

        let first = channel
            .send_draft(&SendMessage::new(
                format!("zeroclaw draft lifecycle smoke {stamp} first"),
                &room_id,
            ))
            .await
            .expect("send first draft")
            .expect("partial mode returns first draft event id");
        let second = channel
            .send_draft(&SendMessage::new(
                format!("zeroclaw draft lifecycle smoke {stamp} second"),
                &room_id,
            ))
            .await
            .expect("send second draft")
            .expect("partial mode returns second draft event id");
        assert_ne!(first, second);

        let first_key = streaming_key(&room_id, &first).expect("first draft key");
        let second_key = streaming_key(&room_id, &second).expect("second draft key");
        {
            let state = channel.streaming_state.read().await;
            assert!(state.partial.contains_key(&first_key));
            assert!(state.partial.contains_key(&second_key));
        }

        tokio::time::sleep(Duration::from_millis(60)).await;
        let first_update = format!("zeroclaw draft lifecycle smoke {stamp} first update");
        channel
            .update_draft(&room_id, &first, &first_update)
            .await
            .expect("update first draft by id");
        {
            let state = channel.streaming_state.read().await;
            assert_eq!(
                state
                    .partial
                    .get(&first_key)
                    .map(|draft| draft.last_text.as_str()),
                Some(first_update.as_str())
            );
            assert!(state.partial.contains_key(&second_key));
        }

        channel
            .finalize_draft(
                &room_id,
                &second,
                &format!("zeroclaw draft lifecycle smoke {stamp} second final"),
                false,
            )
            .await
            .expect("finalize second draft by id");
        {
            let state = channel.streaming_state.read().await;
            assert!(state.partial.contains_key(&first_key));
            assert!(!state.partial.contains_key(&second_key));
        }

        channel
            .cancel_draft(&room_id, &first)
            .await
            .expect("cancel first draft by id");
        {
            let state = channel.streaming_state.read().await;
            assert!(state.partial.is_empty());
        }
    }

    #[tokio::test]
    #[ignore = "requires Matrix smoke credentials and a disposable idle test room"]
    async fn idle_sync_does_not_error_at_30s_cadence() {
        let homeserver = env_first(
            "ZEROCLAW_MATRIX_SMOKE_HOMESERVER",
            "ZEROCLAW_MATRIX_HOMESERVER",
        );
        let room_id = env_first("ZEROCLAW_MATRIX_SMOKE_ROOM_ID", "ZEROCLAW_MATRIX_ROOM_ID");
        let access_token = env_first(
            "ZEROCLAW_MATRIX_SMOKE_ACCESS_TOKEN",
            "ZEROCLAW_MATRIX_ACCESS_TOKEN",
        );

        let idle_secs: u64 = env::var("ZEROCLAW_MATRIX_SMOKE_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(35);
        assert!(
            idle_secs > 30,
            "idle soak must exceed 30s to exercise the pre-fix failure window; got {idle_secs}s"
        );
        let min_longpoll_ms: u64 = env::var("ZEROCLAW_MATRIX_SMOKE_MIN_LONGPOLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000);

        let config = MatrixConfig {
            enabled: true,
            homeserver,
            access_token: Some(access_token),
            allowed_rooms: vec![room_id.clone()],
            stream_mode: StreamMode::Off,
            reply_in_thread: false,
            ack_reactions: Some(false),
            ..MatrixConfig::default()
        };
        let state_dir = TempDir::new().expect("temp state dir");
        let channel = MatrixChannel::new(
            config,
            "matrix",
            Arc::new(Vec::<String>::new),
            state_dir.path().to_path_buf(),
        )
        .expect("matrix channel");

        // Building the client exercises `CLIENT_REQUEST_TIMEOUT` on the
        // underlying `RequestConfig`. If that constant ever regresses below
        // `SYNC_LONGPOLL_TIMEOUT`, the very first long-poll below will
        // error out at the HTTP deadline.
        let client = channel.ensure_client().await.expect("matrix client");

        // Prime the sync token with a single bounded sync_once so the
        // subsequent loop measures true idle long-poll behavior rather
        // than the initial state-fetch round-trip.
        client
            .sync_once(SyncSettings::default().timeout(SYNC_LONGPOLL_TIMEOUT))
            .await
            .expect("initial Matrix sync");

        let soak = Duration::from_secs(idle_secs);
        let min_longpoll = Duration::from_millis(min_longpoll_ms);
        let deadline = Instant::now() + soak;
        let mut call_count: u32 = 0;
        let mut short_longpoll_count: u32 = 0;
        let mut max_call: Duration = Duration::from_millis(0);

        while Instant::now() < deadline {
            let started = Instant::now();
            let result = client
                .sync_once(SyncSettings::default().timeout(SYNC_LONGPOLL_TIMEOUT))
                .await;
            let elapsed = started.elapsed();

            // Primary reviewer assertion: idle `/sync` must not error out.
            // The pre-fix bug surfaced as a request-deadline error at ~30s
            // when the HTTP timeout fired before the long-poll returned.
            result.unwrap_or_else(|e| {
                    panic!(
                        "idle sync_once errored after {elapsed:?} (call #{call_count}); this is the 30s-cadence regression \
                         the PR aims to fix: {e}"
                    )
                });

            call_count += 1;
            if elapsed > max_call {
                max_call = elapsed;
            }
            if elapsed < min_longpoll {
                short_longpoll_count += 1;
            }
        }

        // Defense-in-depth against the other half of the pre-fix bug: a
        // missing `?timeout=` made the homeserver reply instantly, so the
        // SDK would busy-poll. With `SYNC_LONGPOLL_TIMEOUT` set, an idle
        // room should produce only a handful of round-trips per 30s.
        assert!(
            call_count > 0,
            "expected at least one sync_once call during the {idle_secs}s soak"
        );
        assert!(
            max_call >= min_longpoll,
            "every sync_once call returned in <{min_longpoll:?} (max observed: {max_call:?}); \
                 homeserver appears to be replying without honoring `?timeout=` — likely the pre-fix \
                 busy-poll regression. call_count={call_count}"
        );
        // Allow a couple of legitimate early returns (e.g. presence pings)
        // but flag anything that smells like a tight busy-poll loop.
        let busy_poll_budget = ((idle_secs / 5).max(2)) as u32;
        assert!(
            short_longpoll_count <= busy_poll_budget,
            "{short_longpoll_count} of {call_count} sync_once calls returned in <{min_longpoll:?} \
                 (budget for an idle room over {idle_secs}s is {busy_poll_budget}); this matches the \
                 pre-fix busy-poll pattern"
        );

        // Mirror the validation-evidence shape requested on the PR: emit a
        // concise note so a captured `cargo test -- --nocapture` run reads
        // like the reviewer's "short Matrix smoke result" ask.
        eprintln!(
            "matrix idle-sync smoke: soak={idle_secs}s, sync_once_calls={call_count}, \
                 max_call={max_call:?}, short_calls={short_longpoll_count}, no errors at 30s cadence"
        );
    }
}

mod session {
    use super::super::session::{SessionBlob, load, save};
    use tempfile::TempDir;

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let blob = SessionBlob {
            user_id: "@bot:example.org".to_string(),
            device_id: "DEV1".to_string(),
            access_token: "secret".to_string(),
            refresh_token: Some("refresh".to_string()),
        };
        save(dir.path(), &blob).unwrap();
        let loaded = load(dir.path()).unwrap().unwrap();
        assert_eq!(blob, loaded);
    }

    #[test]
    fn missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn corrupt_returns_none() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("session.json");
        std::fs::write(p, "{not valid json").unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_owner_only_perms() {
        // session.json holds the access token in plaintext. On Unix
        // it must be 0o600 regardless of umask so other local users
        // can't read it.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let blob = SessionBlob {
            user_id: "@bot:example.org".to_string(),
            device_id: "DEV1".to_string(),
            access_token: "secret".to_string(),
            refresh_token: None,
        };
        save(dir.path(), &blob).unwrap();
        let meta = std::fs::metadata(dir.path().join("session.json")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected 0o600, got {mode:o}; session.json must be owner-only"
        );
    }
}

mod auth_gating {
    //! Pure-logic tests for the auth-flow gating helpers — keeps
    //! corruption-recovery decisions verifiable without touching the SDK.

    use super::super::client::{
        can_password_relogin, resolve_access_token_identity, saved_session_is_foreign,
        store_has_orphan_data,
    };
    use tempfile::TempDir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };
    use zeroclaw_config::schema::MatrixConfig;

    const WHOAMI_PATH: &str = "/_matrix/client/v3/account/whoami";

    fn cfg(password: Option<&str>, user_id: Option<&str>) -> MatrixConfig {
        MatrixConfig {
            enabled: true,
            homeserver: "https://m.org".into(),
            access_token: None,
            user_id: user_id.map(String::from),
            device_id: None,
            allowed_rooms: vec![],
            interrupt_on_new_message: false,
            stream_mode: Default::default(),
            draft_update_interval_ms: 1500,
            multi_message_delay_ms: 800,
            mention_only: false,
            recovery_key: None,
            password: password.map(String::from),
            approval_timeout_secs: 300,
            reply_in_thread: true,
            ack_reactions: Some(true),
            excluded_tools: vec![],
            reply_min_interval_secs: 0,
            reply_queue_depth_max: 0,
        }
    }

    fn access_token_cfg(homeserver: String) -> MatrixConfig {
        MatrixConfig {
            homeserver,
            access_token: Some("secret-token".into()),
            ..cfg(None, None)
        }
    }

    #[test]
    fn relogin_requires_both_password_and_user_id() {
        assert!(can_password_relogin(&cfg(Some("pw"), Some("@bot:m"))));
        assert!(!can_password_relogin(&cfg(None, Some("@bot:m"))));
        assert!(!can_password_relogin(&cfg(Some("pw"), None)));
        assert!(!can_password_relogin(&cfg(None, None)));
    }

    #[test]
    fn relogin_rejects_empty_strings() {
        assert!(!can_password_relogin(&cfg(Some(""), Some("@bot:m"))));
        assert!(!can_password_relogin(&cfg(Some("pw"), Some(""))));
    }

    fn blob_for(user_id: &str) -> super::super::session::SessionBlob {
        super::super::session::SessionBlob {
            user_id: user_id.to_string(),
            device_id: "DEV1".to_string(),
            access_token: "secret".to_string(),
            refresh_token: None,
        }
    }

    #[test]
    fn foreign_session_detected_when_user_ids_differ() {
        let cfg = cfg(Some("pw"), Some("@clamps-bot:matrix.org"));
        let foreign = blob_for("@bender-bending-rodriguez-zeroclaw:matrix.org");
        assert!(saved_session_is_foreign(&cfg, &foreign));
    }

    #[test]
    fn matching_session_not_foreign() {
        let cfg = cfg(Some("pw"), Some("@clamps-bot:matrix.org"));
        let own = blob_for("@clamps-bot:matrix.org");
        assert!(!saved_session_is_foreign(&cfg, &own));
    }

    #[test]
    fn unset_or_bare_user_id_never_flags() {
        // No configured user_id, or a bare localpart that cannot be
        // compared against the canonical MXID, must not false-positive.
        let any = blob_for("@whoever:matrix.org");
        assert!(!saved_session_is_foreign(&cfg(Some("pw"), None), &any));
        assert!(!saved_session_is_foreign(&cfg(Some("pw"), Some("")), &any));
        assert!(!saved_session_is_foreign(
            &cfg(Some("pw"), Some("clamps-bot")),
            &any
        ));
    }

    #[test]
    fn orphan_detection_no_state_dir() {
        let dir = TempDir::new().unwrap();
        // store/ does not exist
        assert!(!store_has_orphan_data(dir.path()));
    }

    #[test]
    fn orphan_detection_empty_store() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("store")).unwrap();
        assert!(!store_has_orphan_data(dir.path()));
    }

    #[test]
    fn orphan_detection_populated_store() {
        let dir = TempDir::new().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("matrix-sdk-crypto.sqlite3"), b"x").unwrap();
        assert!(store_has_orphan_data(dir.path()));
    }

    #[tokio::test]
    async fn access_token_identity_fetches_missing_user_and_device_from_whoami() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WHOAMI_PATH))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@bot:example.org",
                "device_id": "DEVICE42"
            })))
            .mount(&server)
            .await;

        let identity = resolve_access_token_identity(&access_token_cfg(server.uri()))
            .await
            .unwrap();

        assert_eq!(identity.user_id, "@bot:example.org");
        assert_eq!(identity.device_id.as_deref(), Some("DEVICE42"));
    }

    #[tokio::test]
    async fn access_token_identity_rejects_whoami_without_device_when_not_configured() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WHOAMI_PATH))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@bot:example.org"
            })))
            .mount(&server)
            .await;

        let err = resolve_access_token_identity(&access_token_cfg(server.uri()))
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("whoami response did not include device_id"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn access_token_identity_uses_complete_config_without_whoami() {
        let mut config = access_token_cfg("http://127.0.0.1:9".into());
        config.user_id = Some(" @bot:example.org ".into());
        config.device_id = Some(" DEVICE42 ".into());

        let identity = resolve_access_token_identity(&config).await.unwrap();

        assert_eq!(identity.user_id, "@bot:example.org");
        assert_eq!(identity.device_id.as_deref(), Some("DEVICE42"));
    }

    #[tokio::test]
    async fn access_token_identity_rejects_configured_user_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WHOAMI_PATH))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@actual:example.org",
                "device_id": "DEVICE42"
            })))
            .mount(&server)
            .await;
        let mut config = access_token_cfg(server.uri());
        config.user_id = Some("@configured:example.org".into());

        let err = resolve_access_token_identity(&config).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("does not match Matrix whoami user_id"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn access_token_identity_reports_matrix_error_envelope_without_raw_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WHOAMI_PATH))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "errcode": "M_FORBIDDEN",
                "error": "token rejected",
                "access_token": "secret-token"
            })))
            .mount(&server)
            .await;

        let err = resolve_access_token_identity(&access_token_cfg(server.uri()))
            .await
            .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("M_FORBIDDEN: token rejected"), "{message}");
        assert!(!message.contains("access_token"), "{message}");
        assert!(!message.contains("secret-token"), "{message}");
    }

    #[tokio::test]
    async fn access_token_identity_rejects_configured_device_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WHOAMI_PATH))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@bot:example.org",
                "device_id": "ACTUAL_DEVICE"
            })))
            .mount(&server)
            .await;
        let mut config = access_token_cfg(server.uri());
        config.device_id = Some("CONFIGURED_DEVICE".into());

        let err = resolve_access_token_identity(&config).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("does not match Matrix whoami device_id"),
            "{err}"
        );
    }
}

mod voice {
    use super::super::inbound::is_voice_message;
    use matrix_sdk::event_handler::RawEvent;
    use matrix_sdk::ruma::serde::Raw;

    fn raw(json: serde_json::Value) -> RawEvent {
        let raw: Raw<serde_json::Value> = Raw::new(&json).expect("raw");
        RawEvent(raw.into_json())
    }

    #[test]
    fn audio_with_voice_flag_detected() {
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.audio",
                "body": "voice.ogg",
                "org.matrix.msc3245.voice": {},
            }
        }));
        assert!(is_voice_message(&r));
    }

    #[test]
    fn plain_audio_not_voice() {
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.audio",
                "body": "song.mp3",
            }
        }));
        assert!(!is_voice_message(&r));
    }
}

mod thread_extraction {
    use super::super::inbound::{
        extract_mentions_user_ids, extract_thread_id, interruption_scope_from_anchor,
        resolve_outbound_anchor,
    };
    use matrix_sdk::event_handler::RawEvent;
    use matrix_sdk::ruma::serde::Raw;

    fn raw(json: serde_json::Value) -> RawEvent {
        let raw: Raw<serde_json::Value> = Raw::new(&json).expect("raw");
        RawEvent(raw.into_json())
    }

    #[test]
    fn thread_relation_pulls_root_id() {
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.text",
                "body": "reply",
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": "$root:server",
                }
            }
        }));
        let id = extract_thread_id(&r).expect("some");
        assert_eq!(id.as_str(), "$root:server");
    }

    #[test]
    fn no_relation_returns_none() {
        let r = raw(serde_json::json!({
            "content": { "msgtype": "m.text", "body": "hi" }
        }));
        assert!(extract_thread_id(&r).is_none());
    }

    #[test]
    fn non_thread_relation_returns_none() {
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.text",
                "body": "hi",
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$x:s" }
            }
        }));
        assert!(extract_thread_id(&r).is_none());
    }

    #[test]
    fn root_inbound_starts_new_thread_when_reply_in_thread_enabled() {
        let event_id = "$root:server".parse().expect("event id");
        assert_eq!(
            resolve_outbound_anchor(None, &event_id, true).as_deref(),
            Some("$root:server")
        );
    }

    #[test]
    fn root_inbound_stays_root_when_reply_in_thread_disabled() {
        let event_id = "$root:server".parse().expect("event id");
        assert_eq!(resolve_outbound_anchor(None, &event_id, false), None);
    }

    #[test]
    fn threaded_inbound_keeps_existing_thread_root() {
        let event_id = "$reply:server".parse().expect("event id");
        let thread_root = "$root:server".parse().expect("thread id");
        assert_eq!(
            resolve_outbound_anchor(Some(&thread_root), &event_id, true).as_deref(),
            Some("$root:server")
        );
        assert_eq!(
            resolve_outbound_anchor(Some(&thread_root), &event_id, false).as_deref(),
            Some("$root:server")
        );
    }

    // ── interruption_scope_from_anchor ──────────────────────────

    #[test]
    fn self_anchored_root_strips_interruption_scope() {
        // when reply_in_thread anchors on the inbound event
        // itself the anchor is a delivery detail, not a conversation
        // boundary — interruption_scope_id should be None so
        // cancellation keys match sender+room.
        let event_id = "$ev:server".parse().expect("event id");
        let outbound = resolve_outbound_anchor(None, &event_id, true);
        // thread_ts stays set to the event_id
        assert_eq!(outbound.as_deref(), Some("$ev:server"));
        // interruption_scope_id is stripped
        assert_eq!(
            interruption_scope_from_anchor(outbound.as_deref(), &event_id),
            None
        );
    }

    #[test]
    fn real_thread_reply_preserves_interruption_scope() {
        // A reply inside an existing thread: outbound anchor is the
        // thread root, not the inbound event itself.
        // interruption_scope_id must stay set to the thread root.
        let event_id = "$reply:server".parse().expect("event id");
        let thread_root = "$root:server".parse().expect("thread root");
        let outbound = resolve_outbound_anchor(Some(&thread_root), &event_id, true);
        assert_eq!(outbound.as_deref(), Some("$root:server"));
        assert_eq!(
            interruption_scope_from_anchor(outbound.as_deref(), &event_id).as_deref(),
            Some("$root:server")
        );
    }

    #[test]
    fn no_anchor_yields_no_interruption_scope() {
        // reply_in_thread disabled on a root event: no anchor at all.
        let event_id = "$ev:server".parse().expect("event id");
        let outbound = resolve_outbound_anchor(None, &event_id, false);
        assert_eq!(outbound, None);
        assert_eq!(
            interruption_scope_from_anchor(outbound.as_deref(), &event_id),
            None
        );
    }

    #[test]
    fn mentions_user_ids_extracted() {
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.text",
                "body": "hi",
                "m.mentions": { "user_ids": ["@a:b", "@c:d"] }
            }
        }));
        let ids = extract_mentions_user_ids(&r).expect("some");
        assert_eq!(ids, vec!["@a:b", "@c:d"]);
    }

    #[test]
    fn no_mentions_field_returns_none() {
        let r = raw(serde_json::json!({
            "content": { "msgtype": "m.text", "body": "hi" }
        }));
        assert!(extract_mentions_user_ids(&r).is_none());
    }
}

mod multi_streaming {
    //! `next_paragraph_break` is the heart of MultiMessage streaming —
    //! getting the code-fence detection wrong means agent code blocks
    //! get split mid-block. These cover the corner cases.

    use super::super::streaming::next_paragraph_break;

    #[test]
    fn no_break_returns_none() {
        assert_eq!(next_paragraph_break("hello world"), None);
    }

    #[test]
    fn single_break_at_offset() {
        assert_eq!(next_paragraph_break("first\n\nsecond"), Some(5));
    }

    #[test]
    fn first_break_when_multiple_present() {
        // Caller is expected to consume +2 past the break, so reporting
        // the *first* break is the correct contract — the loop emits one
        // paragraph per iteration.
        assert_eq!(next_paragraph_break("a\n\nb\n\nc"), Some(1));
    }

    #[test]
    fn break_inside_code_fence_ignored() {
        // The `\n\n` after "let x = 1;" is inside ```rust ... ``` and
        // must not be treated as a paragraph boundary.
        let text = "before\n\n```rust\nlet x = 1;\n\nlet y = 2;\n```\n\nafter";
        let break_at = next_paragraph_break(text).expect("first break");
        // First real break is the one between "before" and the fence.
        assert_eq!(&text[..break_at], "before");
    }

    #[test]
    fn break_after_closed_fence_detected() {
        // Once the fence closes, subsequent `\n\n` should be detected.
        let text = "```\ncode\n```\n\nafter";
        assert_eq!(next_paragraph_break(text), Some(12));
    }

    #[test]
    fn fence_must_be_at_line_start() {
        // ``` mid-line is not a fence open — paragraph break still applies.
        let text = "inline ``` not a fence\n\nafter";
        assert!(next_paragraph_break(text).is_some());
    }

    #[test]
    fn unicode_safe() {
        // Byte offset must be on a char boundary so the caller's
        // `text[..break_at]` slice doesn't panic.
        let text = "héllo\n\nwörld";
        let break_at = next_paragraph_break(text).expect("break");
        assert!(text.is_char_boundary(break_at));
        assert_eq!(&text[..break_at], "héllo");
    }
}

mod in_reply_to {
    //! Coverage for the mention-only "@bot can you see this image?"
    //! flow: the inbound text event has no media of its own but its
    //! `m.relates_to.m.in_reply_to.event_id` points at an earlier
    //! media-only event the bot ignored.

    use super::super::inbound::{extract_in_reply_to, parent_media_info};
    use matrix_sdk::event_handler::RawEvent;
    use matrix_sdk::ruma::events::AnySyncTimelineEvent;
    use matrix_sdk::ruma::serde::Raw;

    fn raw(json: serde_json::Value) -> RawEvent {
        let r: Raw<serde_json::Value> = Raw::new(&json).expect("raw");
        RawEvent(r.into_json())
    }

    fn parent_raw(json: serde_json::Value) -> Raw<AnySyncTimelineEvent> {
        Raw::new(&json).expect("parent raw").cast_unchecked()
    }

    #[test]
    fn in_reply_to_extracted_from_plain_reply() {
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.text",
                "body": "@bot can you see this?",
                "m.relates_to": {
                    "m.in_reply_to": { "event_id": "$parent:server" }
                }
            }
        }));
        let id = extract_in_reply_to(&r).expect("some");
        assert_eq!(id.as_str(), "$parent:server");
    }

    #[test]
    fn in_reply_to_extracted_from_threaded_reply() {
        // Modern threaded replies nest m.in_reply_to *inside* the
        // m.thread relation — extract_in_reply_to should handle both.
        let r = raw(serde_json::json!({
            "content": {
                "msgtype": "m.text",
                "body": "...",
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": "$root:server",
                    "m.in_reply_to": { "event_id": "$parent:server" }
                }
            }
        }));
        let id = extract_in_reply_to(&r).expect("some");
        assert_eq!(id.as_str(), "$parent:server");
    }

    #[test]
    fn no_relation_returns_none() {
        let r = raw(serde_json::json!({
            "content": { "msgtype": "m.text", "body": "hi" }
        }));
        assert!(extract_in_reply_to(&r).is_none());
    }

    #[test]
    fn parent_image_plain_url() {
        let p = parent_raw(serde_json::json!({
            "content": {
                "msgtype": "m.image",
                "body": "cat.jpg",
                "url": "mxc://example.org/abc",
                "info": { "mimetype": "image/jpeg" }
            }
        }));
        let info = parent_media_info(p).expect("media info");
        assert!(matches!(
            info.kind,
            super::super::inbound::MediaCategory::Image
        ));
        assert_eq!(info.file_name, "cat.jpg");
        assert_eq!(info.mime.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn parent_voice_distinguished_from_audio() {
        let p = parent_raw(serde_json::json!({
            "content": {
                "msgtype": "m.audio",
                "body": "voice.ogg",
                "url": "mxc://example.org/v",
                "org.matrix.msc3245.voice": {}
            }
        }));
        let info = parent_media_info(p).expect("media info");
        assert!(matches!(
            info.kind,
            super::super::inbound::MediaCategory::Voice
        ));
    }

    #[test]
    fn parent_audio_without_voice_flag_is_audio() {
        let p = parent_raw(serde_json::json!({
            "content": {
                "msgtype": "m.audio",
                "body": "song.mp3",
                "url": "mxc://example.org/m"
            }
        }));
        let info = parent_media_info(p).expect("media info");
        assert!(matches!(
            info.kind,
            super::super::inbound::MediaCategory::Audio
        ));
    }

    #[test]
    fn parent_encrypted_file_decoded() {
        // The `file` key (instead of `url`) signals encrypted media —
        // parent_media_info must decode it as MediaSource::Encrypted.
        let p = parent_raw(serde_json::json!({
            "content": {
                "msgtype": "m.image",
                "body": "secret.jpg",
                "info": { "mimetype": "image/jpeg" },
                "file": {
                    "url": "mxc://example.org/enc",
                    "v": "v2",
                    "key": {
                        "kty": "oct",
                        "alg": "A256CTR",
                        "ext": true,
                        "k": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
                        "key_ops": ["encrypt", "decrypt"]
                    },
                    "iv": "AAAAAAAAAAAAAAAAAAAAAA",
                    "hashes": { "sha256": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8" }
                }
            }
        }));
        let info = parent_media_info(p).expect("media info");
        assert!(matches!(
            info.kind,
            super::super::inbound::MediaCategory::Image
        ));
        assert!(matches!(
            info.source,
            matrix_sdk::ruma::events::room::MediaSource::Encrypted(_)
        ));
    }

    #[test]
    fn parent_text_event_returns_none() {
        let p = parent_raw(serde_json::json!({
            "content": { "msgtype": "m.text", "body": "hi" }
        }));
        assert!(parent_media_info(p).is_none());
    }
}

mod cron_recipient {
    //! Cron operators sometimes write `delivery.to` as `<sender>||<room>`.
    //! `client::normalize_recipient` extracts the last `!`/`#`-prefixed
    //! segment and signals whether it changed anything.

    use super::super::client::normalize_recipient;

    #[test]
    fn plain_room_id_unchanged() {
        let (out, normalized) = normalize_recipient("!abc:server");
        assert_eq!(out, "!abc:server");
        assert!(!normalized);
    }

    #[test]
    fn plain_alias_unchanged() {
        let (out, normalized) = normalize_recipient("#room:server");
        assert_eq!(out, "#room:server");
        assert!(!normalized);
    }

    #[test]
    fn sender_pipe_room_extracts_room() {
        let (out, normalized) = normalize_recipient("@bot:server||!abc:server");
        assert_eq!(out, "!abc:server");
        assert!(normalized);
    }

    #[test]
    fn whitespace_around_pipes_trimmed() {
        let (out, _) = normalize_recipient("@bot:server || !abc:server ");
        assert_eq!(out, "!abc:server");
    }

    #[test]
    fn no_room_segment_falls_through_to_input() {
        // If nothing in the split looks like a room, return the original
        // so resolve_room's downstream parser produces a clear error.
        let (out, normalized) = normalize_recipient("alice||bob");
        assert_eq!(out, "alice||bob");
        assert!(normalized);
    }

    #[test]
    fn last_room_segment_wins() {
        let (out, _) = normalize_recipient("!old:s||!new:s");
        assert_eq!(out, "!new:s");
    }
}

mod outbound_sandbox {
    //! Trust-boundary tests for `outbound::validate_marker_target`. The
    //! marker target string comes from agent text and is therefore
    //! untrusted; the sandbox must keep local reads inside `workspace_dir`
    //! and refuse non-http(s) schemes outright.

    use super::super::outbound::{MarkerTarget, validate_marker_target};
    use tempfile::TempDir;

    #[test]
    fn accepts_workspace_path() {
        let workspace = TempDir::new().unwrap();
        let inside = workspace.path().join("photo.jpg");
        std::fs::write(&inside, b"x").unwrap();
        let result = validate_marker_target(inside.to_str().unwrap(), Some(workspace.path()));
        match result.expect("validate") {
            MarkerTarget::Local(p) => {
                assert!(p.starts_with(std::fs::canonicalize(workspace.path()).unwrap()));
            }
            _ => panic!("expected Local"),
        }
    }

    #[test]
    fn accepts_relative_workspace_path() {
        let workspace = TempDir::new().unwrap();
        let inside = workspace.path().join("photo.jpg");
        std::fs::write(&inside, b"x").unwrap();
        // Relative-to-workspace target — no `./` prefix; mimics the form
        // an agent emits when it knows the workspace as cwd.
        let result = validate_marker_target("photo.jpg", Some(workspace.path()));
        match result.expect("validate") {
            MarkerTarget::Local(_) => {}
            _ => panic!("expected Local"),
        }
    }

    #[test]
    fn rejects_absolute_outside_workspace() {
        let workspace = TempDir::new().unwrap();
        // `/etc/hosts` exists on every Linux host; we don't actually
        // read it, just canonicalise.
        let result = validate_marker_target("/etc/hosts", Some(workspace.path()));
        assert!(result.is_err(), "expected Err for /etc target");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("outside workspace_dir"),
            "expected 'outside workspace_dir' in error, got: {msg}"
        );
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let workspace = TempDir::new().unwrap();
        let parent = workspace.path().parent().unwrap();
        let outside_dir = parent.join("zeroclaw-test-outside");
        let _ = std::fs::create_dir(&outside_dir);
        let outside_file = outside_dir.join("secret");
        std::fs::write(&outside_file, b"x").unwrap();
        let traversal = format!(
            "../{}/secret",
            outside_dir.file_name().unwrap().to_str().unwrap()
        );
        let result = validate_marker_target(&traversal, Some(workspace.path()));
        let _ = std::fs::remove_file(&outside_file);
        let _ = std::fs::remove_dir(&outside_dir);
        assert!(
            result.is_err(),
            "expected Err for `..` traversal escaping workspace"
        );
    }

    #[test]
    fn rejects_file_scheme() {
        let workspace = TempDir::new().unwrap();
        let result = validate_marker_target("file:///etc/hosts", Some(workspace.path()));
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("disallowed scheme"),
            "expected scheme rejection, got: {msg}"
        );
    }

    #[test]
    fn rejects_data_scheme() {
        let workspace = TempDir::new().unwrap();
        let result = validate_marker_target("data:text/plain;base64,aGk=", Some(workspace.path()));
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("disallowed scheme"),
            "expected scheme rejection, got: {msg}"
        );
    }

    #[test]
    fn rejects_unknown_scheme() {
        let workspace = TempDir::new().unwrap();
        let result = validate_marker_target("ftp://example.com/x", Some(workspace.path()));
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("disallowed scheme"),
            "expected scheme rejection, got: {msg}"
        );
    }

    #[test]
    fn accepts_http_url() {
        let workspace = TempDir::new().unwrap();
        let result = validate_marker_target("http://example.com/photo.jpg", Some(workspace.path()));
        match result.expect("validate") {
            MarkerTarget::Http(u) => assert_eq!(u.scheme(), "http"),
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn accepts_https_url() {
        let workspace = TempDir::new().unwrap();
        let result =
            validate_marker_target("https://example.com/photo.jpg", Some(workspace.path()));
        match result.expect("validate") {
            MarkerTarget::Http(u) => assert_eq!(u.scheme(), "https"),
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn local_path_without_workspace_is_refused() {
        // Operator forgot to wire `with_workspace_dir`. Local marker
        // cannot be safely resolved — refuse rather than fall back to
        // process cwd (which would be the daemon working dir, not the
        // workspace).
        let result = validate_marker_target("photo.jpg", None);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("without a workspace_dir"),
            "expected workspace_dir-not-configured error, got: {msg}"
        );
    }

    #[test]
    fn http_url_works_without_workspace() {
        // HTTP URLs don't depend on a workspace — they should succeed
        // even when workspace_dir is None.
        let result = validate_marker_target("https://example.com/x.jpg", None);
        assert!(matches!(result, Ok(MarkerTarget::Http(_))));
    }

    fn assert_ssr_refused(target: &str) {
        let workspace = TempDir::new().unwrap();
        let result = validate_marker_target(target, Some(workspace.path()));
        let msg = result
            .expect_err(&format!("expected SSRF refusal for {target}"))
            .to_string();
        assert!(
            msg.contains("private or local host"),
            "expected SSRF refusal message for {target}, got: {msg}"
        );
    }

    #[test]
    fn ssrf_refuses_loopback_v4() {
        assert_ssr_refused("http://127.0.0.1/admin");
    }

    #[test]
    fn ssrf_refuses_loopback_v6() {
        assert_ssr_refused("http://[::1]/admin");
    }

    #[test]
    fn ssrf_refuses_rfc1918_v4() {
        for h in ["10.0.0.5", "172.16.0.1", "192.168.1.1"] {
            assert_ssr_refused(&format!("http://{h}/internal"));
        }
    }

    #[test]
    fn ssrf_refuses_link_local_v4() {
        // AWS / GCP / Azure cloud-metadata endpoint.
        assert_ssr_refused("http://169.254.169.254/latest/meta-data/");
    }

    #[test]
    fn ssrf_refuses_cgnat_v4() {
        // RFC 6598 shared address space (100.64.0.0/10).
        assert_ssr_refused("http://100.64.0.1/api");
    }

    #[test]
    fn ssrf_refuses_ipv4_mapped_loopback() {
        // ::ffff:127.0.0.1 — IPv4-mapped loopback, often missed by
        // naive IP-literal checks.
        assert_ssr_refused("http://[::ffff:127.0.0.1]/admin");
    }

    #[test]
    fn ssrf_refuses_localhost_name() {
        assert_ssr_refused("http://localhost/admin");
        assert_ssr_refused("http://foo.localhost/admin");
    }

    #[test]
    fn ssrf_refuses_local_suffix_name() {
        assert_ssr_refused("http://printer.local/");
    }

    #[test]
    fn ssrf_refuses_ipv6_unique_local() {
        // fc00::/7 — RFC 4193 unique local addresses.
        assert_ssr_refused("http://[fd00::1]/");
    }

    #[test]
    fn ssrf_refuses_ipv6_link_local() {
        // fe80::/10 — link-local.
        assert_ssr_refused("http://[fe80::1]/");
    }

    #[test]
    fn ssrf_refuses_https_same_as_http() {
        // The guard is scheme-agnostic; the same private host is
        // refused over https:// too.
        assert_ssr_refused("https://10.0.0.5/secret");
        assert_ssr_refused("https://[::1]/secret");
    }

    #[test]
    fn ssrf_refuses_with_userinfo_attempt() {
        // An attacker who controls the host string might smuggle a
        // private host through userinfo syntax; the SSRF guard fires
        // on the host portion regardless.
        assert_ssr_refused("http://attacker@127.0.0.1/");
    }

    #[test]
    fn accepts_public_host() {
        // Sanity: a normal public-looking host must still pass through.
        for h in ["example.com", "cdn.example.com", "1.1.1.1", "8.8.8.8"] {
            let workspace = TempDir::new().unwrap();
            let result =
                validate_marker_target(&format!("https://{h}/photo.jpg"), Some(workspace.path()));
            assert!(
                matches!(result, Ok(MarkerTarget::Http(_))),
                "public host {h} must be accepted, got: {result:?}"
            );
        }
    }
}

mod outbound_redirect_ssrf {

    use super::super::outbound::fetch_http;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn private_redirect_body(location: &str) -> ResponseTemplate {
        ResponseTemplate::new(302).insert_header("Location", location)
    }

    #[tokio::test]
    async fn rejects_redirect_to_cloud_metadata_ip() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(private_redirect_body(
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let url = reqwest::Url::parse(&format!("{}/photo.jpg", server.uri())).unwrap();
        let err: anyhow::Error = fetch_http(url)
            .await
            .expect_err("redirect to cloud-metadata IP must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("private or local host") || msg.contains("PermissionDenied"),
            "expected SSRF redirect refusal, got: {msg}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn rejects_redirect_to_loopback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(private_redirect_body("http://127.0.0.1:9200/_cat/indices"))
            .expect(1)
            .mount(&server)
            .await;

        let url = reqwest::Url::parse(&format!("{}/photo.jpg", server.uri())).unwrap();
        let err: anyhow::Error = fetch_http(url)
            .await
            .expect_err("redirect to loopback must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("private or local host") || msg.contains("PermissionDenied"),
            "expected SSRF redirect refusal, got: {msg}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn follows_public_redirect_target() {
        assert!(
            !zeroclaw_tools::helpers::domain_guard::is_private_or_local_host("example.com"),
            "public hostnames must not be classified as private by the per-hop guard"
        );
        assert!(
            !zeroclaw_tools::helpers::domain_guard::is_private_or_local_host("cdn.example.com"),
            "public subdomains must not be classified as private"
        );
    }
}

mod transcription_gate {

    use super::super::inbound::{MediaCategory, should_transcribe};
    use zeroclaw_config::schema::TranscriptionConfig;

    fn enabled_cfg() -> TranscriptionConfig {
        // Construct via Default + struct update so we stay robust to
        // future field additions on TranscriptionConfig.
        TranscriptionConfig {
            enabled: true,
            ..TranscriptionConfig::default()
        }
    }

    fn disabled_cfg() -> TranscriptionConfig {
        TranscriptionConfig::default()
    }

    #[test]
    fn voice_with_enabled_cfg_transcribes() {
        assert!(should_transcribe(
            &MediaCategory::Voice,
            Some(&enabled_cfg())
        ));
    }

    #[test]
    fn voice_with_disabled_cfg_does_not_transcribe() {
        assert!(!should_transcribe(
            &MediaCategory::Voice,
            Some(&disabled_cfg())
        ));
    }

    #[test]
    fn voice_without_cfg_does_not_transcribe() {
        assert!(!should_transcribe(&MediaCategory::Voice, None));
    }

    #[test]
    fn audio_with_enabled_cfg_does_not_transcribe() {
        // Plain m.audio (no MSC3245 voice flag) is left as a regular
        // audio file — only voice notes get transcribed.
        assert!(!should_transcribe(
            &MediaCategory::Audio,
            Some(&enabled_cfg())
        ));
    }

    #[test]
    fn image_with_enabled_cfg_does_not_transcribe() {
        assert!(!should_transcribe(
            &MediaCategory::Image,
            Some(&enabled_cfg())
        ));
    }

    #[test]
    fn voice_kind_alone_is_sufficient() {
        assert!(should_transcribe(
            &MediaCategory::Voice,
            Some(&enabled_cfg())
        ));
    }
}

mod outbound_send_outcome {
    //! Decision logic for what `outbound::send` does after attachment
    //! uploads complete. Marker-only messages used to error even though
    //! the attachment had landed; this captures the new contract.

    use super::super::outbound::{SendOutcome, decide_send_outcome};

    #[test]
    fn non_empty_text_with_attachment_sends_text() {
        assert_eq!(decide_send_outcome(false, true), SendOutcome::SendText);
    }

    #[test]
    fn non_empty_text_without_attachment_sends_text() {
        assert_eq!(decide_send_outcome(false, false), SendOutcome::SendText);
    }

    #[test]
    fn empty_text_with_attachment_returns_attachment() {
        // The bug fix: marker-only sends must surface the attachment's
        // event_id, not an error.
        assert_eq!(
            decide_send_outcome(true, true),
            SendOutcome::ReturnAttachment
        );
    }

    #[test]
    fn empty_text_without_attachment_is_error() {
        // True empty-message case: nothing to deliver, surface the error.
        assert_eq!(decide_send_outcome(true, false), SendOutcome::EmptyError);
    }
}

mod outbound_attachment_info {
    use super::super::outbound::{AttachmentKind, attachment_config_for};
    use matrix_sdk::{attachment::AttachmentInfo, ruma::UInt};
    use zeroclaw_api::media::MediaAttachment;

    fn attachment(file_name: &str, mime_type: &str, len: usize) -> MediaAttachment {
        MediaAttachment {
            file_name: file_name.to_string(),
            data: vec![0; len],
            mime_type: Some(mime_type.to_string()),
        }
    }

    fn info_size(info: AttachmentInfo) -> Option<UInt> {
        match info {
            AttachmentInfo::Image(info) => info.size,
            AttachmentInfo::Video(info) => info.size,
            AttachmentInfo::Audio(info) | AttachmentInfo::Voice(info) => info.size,
            AttachmentInfo::File(info) => info.size,
        }
    }

    #[test]
    fn structured_file_attachment_carries_matrix_size_info() {
        let att = attachment("report.pdf", "application/pdf", 4096);

        let mime = super::super::outbound::attachment_mime(&att);
        let config = attachment_config_for(&att, AttachmentKind::Auto, &mime, None);

        let info = config.info.expect("attachment info is populated");
        assert!(matches!(info, AttachmentInfo::File(_)));
        assert_eq!(info_size(info), UInt::try_from(4096usize).ok());
    }

    #[test]
    fn media_markers_use_type_specific_matrix_info_with_size() {
        let cases = [
            (
                AttachmentKind::Image,
                attachment("photo.png", "image/png", 17),
                "image",
            ),
            (
                AttachmentKind::Audio,
                attachment("clip.ogg", "audio/ogg", 23),
                "audio",
            ),
            (
                AttachmentKind::Video,
                attachment("movie.mp4", "video/mp4", 31),
                "video",
            ),
        ];

        for (kind, att, expected_kind) in cases {
            let mime = super::super::outbound::attachment_mime(&att);
            let config = attachment_config_for(&att, kind, &mime, None);
            let info = config.info.expect("attachment info is populated");
            match (&info, expected_kind) {
                (AttachmentInfo::Image(_), "image") => {}
                (AttachmentInfo::Audio(_), "audio") => {}
                (AttachmentInfo::Video(_), "video") => {}
                _ => panic!("unexpected attachment info kind {info:?}"),
            }
            assert_eq!(info_size(info), UInt::try_from(att.data.len()).ok());
        }
    }

    #[test]
    fn attachment_info_kind_matches_final_mime_type() {
        let image_named_as_file = attachment("photo.png", "image/png", 47);
        let mime = super::super::outbound::attachment_mime(&image_named_as_file);
        let config = attachment_config_for(&image_named_as_file, AttachmentKind::File, &mime, None);
        let info = config.info.expect("attachment info is populated");
        assert!(
            matches!(info, AttachmentInfo::Image(_)),
            "info must match the MIME-selected Matrix event type"
        );
        assert_eq!(
            info_size(info),
            UInt::try_from(image_named_as_file.data.len()).ok()
        );

        let image_marker_with_file_mime = attachment("report.pdf", "application/pdf", 53);
        let mime = super::super::outbound::attachment_mime(&image_marker_with_file_mime);
        let config = attachment_config_for(
            &image_marker_with_file_mime,
            AttachmentKind::Image,
            &mime,
            None,
        );
        let info = config.info.expect("attachment info is populated");
        assert!(
            matches!(info, AttachmentInfo::File(_)),
            "file MIME should use file info so SDK preserves size"
        );
        assert_eq!(
            info_size(info),
            UInt::try_from(image_marker_with_file_mime.data.len()).ok()
        );
    }
}
