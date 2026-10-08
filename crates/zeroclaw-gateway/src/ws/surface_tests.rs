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
async fn surfaces_share_identity_soul_voice_and_history_without_pinning_the_first_socket() {
    use zeroclaw_gateway_client::{Client, ConnectOptions, Frame};
    use zeroclaw_memory::companion::{SoulPrinciples, SoulProfileStore, SoulVoice};
    let mut chat = SharedChat::new();
    let data_dir = chat._tmp.path().join("owner-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let soul = SoulProfileStore::shared(&data_dir).unwrap();
    soul.set_principles("web", SoulPrinciples { items: vec!["Keep owner authority explicit.".into()] }, 0, 1).unwrap();
    soul.set_voice("web", SoulVoice { heads: [("warmth".into(), zeroclaw_config::persona::PersonaLevel::High)].into() }, 0, 1).unwrap();
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
