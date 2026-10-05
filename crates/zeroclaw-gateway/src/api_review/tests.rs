use super::*;
use axum::routing::post;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_memory::companion::reflection::{OwnerMessages, reflect};
use zeroclaw_memory::companion::{GrowthKind, SoulGrowth, UserModelKind};

struct FakeReflection;
#[async_trait::async_trait]
impl ModelProvider for FakeReflection {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _input: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        Ok(r#"{"proposals":[{"layer":"growth","growth_kind":"bond","proposal":"Shared shorthand"}],"user_model_candidates":[{"kind":"preference","statement":"Concise replies","semantic_key":"communication.short","evidence_indices":[0]}]}"#.into())
    }
}
impl zeroclaw_api::attribution::Attributable for FakeReflection {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        use zeroclaw_api::attribution::*;
        Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
    }
    fn alias(&self) -> &str {
        "reflection-fixture"
    }
}
fn fixture() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = zeroclaw_config::schema::Config {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    config.agents.insert(
        "nova".into(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            ..Default::default()
        },
    );
    config.nodes.auth_token = Some("node-token".into());
    let mut state = crate::api::test_state(config);
    state.pairing = Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
        false,
        &["operator-token".into()],
    ));
    (dir, state)
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn server(state: AppState) -> (Server, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = routes()
        .route(
            "/api/user-model/candidates/{id}/review",
            post(crate::api_user_model::review_candidate),
        )
        .route(
            "/api/soul/proposals/{id}/resolve",
            post(crate::api_soul::post_resolve_proposal),
        )
        .with_state(state);
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (Server(handle), address)
}
async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let raw = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = Vec::new(); stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (header, body) = response.split_once("\r\n\r\n").unwrap();
        let status = header.split_whitespace().nth(1).unwrap().parse::<u16>().unwrap();
        (StatusCode::from_u16(status).unwrap(), serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({"raw":body})))
    }).await.unwrap()
}
const OPERATOR: Option<&str> = Some("operator-token");

#[tokio::test]
async fn actual_http_reflection_inbox_review_reword_narrow_dismiss_and_reopen() {
    let (dir, state) = fixture();
    let soul = SoulProfileStore::open(dir.path()).unwrap();
    soul.ensure_seeded("nova", "Nova", 100).unwrap();
    let before = soul.profile("nova").unwrap();
    let user = UserModelStore::open(dir.path()).unwrap();
    let messages = OwnerMessages {
        messages: vec![zeroclaw_api::review::ReflectionMessage {
            source: zeroclaw_api::review::UserMessageSource::Operator,
            session_id: "session-a".into(),
            at_unix: 101,
            text: "Please use concise replies and our shorthand".into(),
        }],
    };
    let receipt = reflect(
        &soul,
        "nova",
        &user,
        Default::default(),
        &messages,
        || {
            Ok((
                Box::new(FakeReflection) as Box<dyn ModelProvider>,
                "fake".into(),
            ))
        },
        100,
        102,
    )
    .await
    .unwrap();
    assert_eq!(
        (
            receipt.proposals_created,
            receipt.user_model_candidates_created
        ),
        (1, 1)
    );
    assert_eq!(soul.profile("nova").unwrap(), before);
    assert!(user.active_heads(None).unwrap().is_empty());
    let candidate = user.list_pending_candidates().unwrap().remove(0);
    let (_server, address) = server(state).await;
    for token in [None, Some("node-token"), Some("bridge-token")] {
        assert_eq!(
            request(address, "GET", "/api/review/inbox", token, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, inbox) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inbox["total"], 3);
    let items = inbox["items"].as_array().unwrap();
    let proposal = items.iter().find(|i| i["kind"] == "soul_proposal").unwrap();
    let user_item = items
        .iter()
        .find(|i| i["kind"] == "user_model_candidate")
        .unwrap();
    let receipt = items
        .iter()
        .find(|i| i["kind"] == "reflection_receipt")
        .unwrap();
    assert_eq!(receipt["item"]["user_model_candidates_created"], 1);
    assert_eq!(user_item["item"]["evidence"], candidate.evidence);
    assert_eq!(request(address,"POST",proposal["review_url"].as_str().unwrap(),OPERATOR,Some(serde_json::json!({"agent":"nova","resolution":"accepted","final_text":"Owner chosen shorthand"}))).await.0,StatusCode::OK);
    let review_path = user_item["review_url"].as_str().unwrap();
    let body = serde_json::json!({"action":"narrow","narrowed_scope":"session:session-a","final_text":"Keep replies brief in this session"});
    assert_eq!(
        request(address, "POST", review_path, OPERATOR, Some(body.clone()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(address, "POST", review_path, OPERATOR, Some(body))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let reopened = UserModelStore::open(dir.path()).unwrap();
    let heads = reopened.active_heads(None).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].statement, "Keep replies brief in this session");
    assert_eq!(heads[0].scope, "session:session-a");
    assert_eq!(
        heads[0].source_candidate.as_deref(),
        Some(candidate.id.as_str())
    );
    let (original, decisions) = reopened.candidate_history(&candidate.id).unwrap().unwrap();
    assert_eq!(original, candidate);
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        SoulProfileStore::open(dir.path())
            .unwrap()
            .profile("nova")
            .unwrap()
            .growth
            .unwrap()
            .value,
        SoulGrowth {
            entries: vec![zeroclaw_memory::companion::GrowthEntry {
                kind: GrowthKind::Bond,
                text: "Owner chosen shorthand".into()
            }]
        }
    );
    let rejected = user
        .record_observation(
            UserModelKind::Habit,
            "Unwanted suggestion",
            "habit.example",
            "[]",
            104,
        )
        .unwrap();
    assert_eq!(
        request(
            address,
            "POST",
            &format!("/api/user-model/candidates/{}/review", rejected.id),
            OPERATOR,
            Some(serde_json::json!({"action":"reject"}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(reopened.active_heads(None).unwrap().len(), 1);
    assert_eq!(
        reopened.candidate_history(&rejected.id).unwrap().unwrap().0,
        rejected
    );
    let (_, after) = request(address, "GET", "/api/review/inbox", OPERATOR, None).await;
    assert_eq!(after["total"], 1);
    assert_eq!(after["items"][0]["kind"], "reflection_receipt");
}

#[tokio::test]
async fn unavailable_store_and_invalid_queries_return_errors() {
    let (dir, state) = fixture();
    let (_server, address) = server(state).await;
    for suffix in ["?limit=0", "?limit=201", "?offset=bad", "?unknown=1"] {
        assert_eq!(
            request(
                address,
                "GET",
                &format!("/api/review/inbox{suffix}"),
                OPERATOR,
                None
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(
            address,
            "GET",
            "/api/review/inbox?agent=unknown",
            OPERATOR,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    std::fs::create_dir(dir.path().join("soul.db")).unwrap();
    assert_eq!(
        request(address, "GET", "/api/review/inbox", OPERATOR, None)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
