use super::*;
use serde_json::json;

#[test]
fn surface_registration_is_closed_and_conflicts_are_rejected() {
    assert_eq!(surface::resolve(None, None), Ok(None));
    for (name, register) in [("web", ChatSurface::Web), ("telegram", ChatSurface::Telegram), ("cli", ChatSurface::Cli)] {
        assert_eq!(surface::resolve(None, Some(name)), Ok(Some(register)));
        assert_eq!(surface::resolve(Some(register), Some(name)), Ok(Some(register)));
    }
    let query = axum::extract::Query::<WsQuery>::try_from_uri(&"/ws/chat?agent=web&surface=web".parse().unwrap()).unwrap();
    assert_eq!(surface::resolve(None, query.surface.as_deref()), Ok(Some(ChatSurface::Web)));
    assert!(surface::resolve(Some(ChatSurface::Web), Some("telegram")).is_err());
    for value in [json!(null), json!(17), json!(true), json!([]), json!({}), json!("other"), json!("Web")] {
        assert!(surface::connect_value(None, &json!({"type":"connect", "surface":value})).is_err());
    }
}

fn without_surface(prompt: &str) -> String {
    prompt.split("\n\n").filter(|part| *part != "## Surface" && !part.starts_with("Web: use ") && !part.starts_with("Telegram: keep ") && !part.starts_with("CLI: use ")).collect::<Vec<_>>().join("\n\n")
}

/// Actual TCP route, connect frames and provider-bound prompts on one retained Agent.
#[tokio::test]
async fn surfaces_share_identity_soul_tone_and_history_without_pinning_the_first_socket() {
    use zeroclaw_gateway_client::{Client, ConnectOptions, Frame};
    use zeroclaw_memory::companion::{SoulPrinciples, SoulProfileStore, SoulTone};
    let mut chat = SharedChat::new();
    let data_dir = chat._tmp.path().join("owner-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let soul = SoulProfileStore::shared(&data_dir).unwrap();
    soul.set_principles("web", SoulPrinciples { items: vec!["Keep owner authority explicit.".into()] }, 0, 1).unwrap();
    soul.set_tone("web", SoulTone { heads: [("warmth".into(), zeroclaw_config::persona::PersonaLevel::High)].into() }, 0, 1).unwrap();
    chat.owner_config = Some(Arc::new(zeroclaw_config::schema::Config { data_dir, ..Default::default() }));
    {
        let mut config = chat.state.config.write();
        config.agents.insert("web".into(), zeroclaw_config::schema::AliasedAgentConfig { enabled: true, ..Default::default() });
        config.providers.models.openai.insert("default".into(), zeroclaw_config::schema::OpenAIModelProviderConfig { base: zeroclaw_config::schema::ModelProviderConfig { model: Some("test-model".into()), ..Default::default() } });
    }
    let _owner = chat.attach_key("gw_shared\u{1f}web").await;
    chat.gate.add_permits(5);
    let app = axum::Router::new().route("/ws/chat", axum::routing::get(handle_ws_chat)).with_state(chat.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = ConnectOptions { gateway: format!("ws://{}", listener.local_addr().unwrap()), agent: "web".into(), session_id: Some("shared".into()), token: None };
    let exchange = async {
        let invalid = Client::connect_with_surface(&options, "other").await;
        assert!(invalid.is_err());
        assert!(chat.seen.lock().is_empty());
        for register in [Some("web"), Some("telegram"), Some("cli"), None, Some("web")] {
            let mut client = match register {
                Some(name) => Client::connect_with_surface(&options, name).await.unwrap(),
                None => Client::connect(&options).await.unwrap(),
            };
            client.send_message("same question").await.unwrap();
            loop {
                match client.next_frame().await.unwrap().unwrap() {
                    Frame::Done { .. } => break,
                    Frame::Error { code, .. } => panic!("surface turn failed: {code:?}"),
                    _ => {}
                }
            }
            client.close().await.unwrap();
        }
        let prompts = chat.systems.lock();
        assert_eq!(prompts.len(), 5);
        assert!(prompts[0].contains("You are web."));
        assert!(prompts[0].contains("warmth"));
        assert!(prompts[0].contains("Keep owner authority explicit."));
        let baseline = &prompts[3];
        assert!(!baseline.contains("## Surface"));
        for (index, register) in [(0, "Web:"), (1, "Telegram:"), (2, "CLI:"), (4, "Web:")] {
            assert_eq!(prompts[index].matches("## Surface").count(), 1);
            assert!(prompts[index].contains(&format!("## Surface\n\n{register}")));
            assert_eq!(without_surface(&prompts[index]), *baseline, "all non-Surface prompt bytes must remain identical");
        }
        assert_eq!(chat.seen.lock().iter().map(Vec::len).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
    };
    tokio::select! {
        result = axum::serve(listener, app.into_make_service()) => panic!("server stopped: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(15), exchange) => result.unwrap(),
    }
}

#[test]
fn voice_registration_requires_explicit_build_opt_in() {
    let expected = if cfg!(feature = "voice-surface") { Ok(Some(ChatSurface::Voice)) } else { Err(()) };
    assert_eq!(surface::resolve(None, Some("voice")), expected);
    assert_eq!(surface::connect_value(None, &json!({"surface":"voice"})), expected);
    assert_eq!(surface::connect_value(Some(ChatSurface::Voice), &json!({})), expected);
    assert!(surface::resolve(Some(ChatSurface::Cli), Some("voice")).is_err());
}

#[test]
fn voice_wire_renders_private_copies_including_replay_without_changing_sequence() {
    let chat = SharedChat::new();
    let canonical = json!({"type":"done", "full_response":"# Hello\n\n**Do not** pay `0`.", "model":"same"});
    let parse = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
    for register in [None, Some(ChatSurface::Web), Some(ChatSurface::Cli)] {
        assert_eq!(surface::wire(&chat.state, register, &canonical.to_string()), canonical.to_string());
    }
    let spoken = parse(surface::wire(&chat.state, Some(ChatSurface::Voice), &canonical.to_string()));
    assert_eq!(spoken["full_response"], "Hello\nDo not pay 0.");
    assert_eq!(spoken["model"], canonical["model"]);
    let replay = json!({"type":"turn_replay","last_seq":2,"terminal":true,"events":[
        {"type":"turn_event","seq":1,"event":{"type":"chunk","content":"**Do"}},
        {"type":"turn_event","seq":2,"event":canonical,"terminal":true}
    ]});
    let rendered = parse(surface::wire(&chat.state, Some(ChatSurface::Voice), &replay.to_string()));
    assert_eq!(rendered["last_seq"], 2);
    assert_eq!(rendered["events"][0]["seq"], 1);
    assert_eq!(rendered["events"][0]["event"]["content"], "");
    assert_eq!(rendered["events"][1]["event"], spoken);
    assert_eq!(replay["events"][0]["event"]["content"], "**Do");
    for frame in [json!({"type":"aborted"}), json!({"type":"error","code":"FAILED"}), json!({"type":"approval_request","id":"a"})] {
        assert_eq!(parse(surface::wire(&chat.state, Some(ChatSurface::Voice), &frame.to_string())), frame);
    }
}

#[cfg(feature = "voice-surface")]
#[tokio::test]
async fn three_surfaces_render_same_turn_without_changing_soul_or_shared_history() {
    use zeroclaw_gateway_client::{Client, ConnectOptions, Frame};
    use zeroclaw_memory::companion::{SoulPrinciples, SoulProfileStore};
    let mut chat = SharedChat::new();
    let data_dir = chat._tmp.path().join("synthetic-owner");
    std::fs::create_dir_all(&data_dir).unwrap();
    let soul = SoulProfileStore::shared(&data_dir).unwrap();
    soul.set_principles("web", SoulPrinciples { items: vec!["Keep owner authority explicit.".into()] }, 0, 1).unwrap();
    soul.ensure_seeded("web", "web", 1).unwrap();
    let before = serde_json::to_value(soul.profile("web").unwrap()).unwrap();
    chat.owner_config = Some(Arc::new(zeroclaw_config::schema::Config { data_dir, ..Default::default() }));
    {
        let mut config = chat.state.config.write();
        config.agents.insert("web".into(), zeroclaw_config::schema::AliasedAgentConfig { enabled: true, ..Default::default() });
        config.providers.models.openai.insert("default".into(), zeroclaw_config::schema::OpenAIModelProviderConfig { base: zeroclaw_config::schema::ModelProviderConfig { model: Some("test-model".into()), ..Default::default() } });
    }
    let retained = chat.attach_key("gw_shared\u{1f}web").await;
    chat.gate.add_permits(3);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = ConnectOptions { gateway: format!("ws://{}", listener.local_addr().unwrap()), agent: "web".into(), session_id: Some("shared".into()), token: None };
    let app = axum::Router::new().route("/ws/chat", axum::routing::get(handle_ws_chat)).with_state(chat.state.clone());
    let exchange = async {
        let mut web = Client::connect_with_surface(&options, "web").await.unwrap();
        let mut cli = Client::connect_with_surface(&options, "cli").await.unwrap();
        let mut voice = Client::connect_with_surface(&options, "voice").await.unwrap();
        let canonical = "# Result\n\n**Do not** pay `0`.\n\n| Item | Count |\n|---|---|\n| 苹果 | 0 |";
        for origin in ["web", "cli", "voice"] {
            let input = format!("echo:{canonical}");
            match origin {
                "web" => web.send_message(&input).await.unwrap(),
                "cli" => cli.send_message(&input).await.unwrap(),
                _ => voice.send_message(&input).await.unwrap(),
            };
            for (name, client) in [("web", &mut web), ("cli", &mut cli), ("voice", &mut voice)] {
                loop {
                    match client.next_frame().await.unwrap().unwrap() {
                        Frame::Done { full_response, .. } => {
                            assert_eq!(full_response, if name == "voice" { zeroclaw_runtime::spoken::render(canonical) } else { canonical.to_string() });
                            break;
                        }
                        Frame::Error { code, .. } => panic!("surface error: {code:?}"),
                        _ => {}
                    }
                }
            }
        }
        {
        let prompts = chat.systems.lock();
        assert_eq!(prompts.len(), 3);
        for prompt in prompts.iter() {
            assert_eq!(without_surface(prompt), prompts[2]);
            assert!(prompt.contains("Keep owner authority explicit."));
        }
        assert_eq!(serde_json::to_value(soul.profile("web").unwrap()).unwrap(), before);
        }
        let session = retained.conversation.agent.lock().await;
        let replies: Vec<_> = session.agent.history().iter().filter_map(|m| match m { zeroclaw_api::model_provider::ConversationMessage::Chat(m) if m.role == "assistant" => Some(m.content.as_str()), _ => None }).collect();
        assert_eq!(replies, vec![canonical; 3]);
        web.close().await.unwrap(); cli.close().await.unwrap(); voice.close().await.unwrap();
    };
    tokio::select! {
        result = axum::serve(listener, app.into_make_service()) => panic!("server stopped: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(20), exchange) => result.unwrap(),
    }
}


#[test]
fn voice_wire_rescans_decoded_and_reassembled_text_with_current_policy() {
    use zeroclaw_runtime::security::outbound::{sanitize_outbound_response, OutboundContentFormat};
    let chat = SharedChat::new();
    let secret = format!("sk-{}", "a".repeat(48));
    for raw in [format!("sk&#45;{}", "a".repeat(48)), format!("sk-**{}**{}", "a".repeat(24), "a".repeat(24))] {
        let policy = chat.state.config.read().security.leak_detection.clone();
        let canonical = sanitize_outbound_response(&raw, &Default::default(), &policy, OutboundContentFormat::Markdown);
        assert!(!canonical.contains(&secret));
        assert!(zeroclaw_runtime::spoken::render(&canonical).contains(&secret));
        let frame = json!({"type":"turn_replay", "events":[{"type":"turn_event","seq":1,"event":{"type":"done","full_response":canonical}}]});
        let rendered = surface::wire(&chat.state, Some(ChatSurface::Voice), &frame.to_string());
        assert!(!rendered.contains(&secret));
        assert_eq!(surface::wire(&chat.state, Some(ChatSurface::Web), &frame.to_string()), frame.to_string());
        chat.state.config.write().security.leak_detection.enabled = false;
        assert!(surface::wire(&chat.state, Some(ChatSurface::Voice), &frame.to_string()).contains(&secret));
        chat.state.config.write().security.leak_detection.enabled = true;
    }
}
