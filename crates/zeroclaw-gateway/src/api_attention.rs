//! Owner-only controls over the existing bridge outbox; never source execution.
use crate::AppState;
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use std::net::SocketAddr;
use zeroclaw_infra::bridge_outbox::BridgeOutbox;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/attention/{bridge}", get(list))
        .route("/api/attention/{bridge}/{id}", get(inspect).post(act))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    recipient: String,
    source_kind: String,
    source_id: String,
    action: String,
    until: Option<i64>,
}

fn failure(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(serde_json::json!({"code": code}))).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    after_seq: i64,
}

async fn list(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(bridge): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    if let Some(error) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return error;
    }
    let data_dir = state.config.read().data_dir.clone();
    match BridgeOutbox::shared(&data_dir).and_then(|outbox| {
        Ok((
            outbox.list(&bridge, query.after_seq, 100)?,
            outbox.mutes(&bridge)?,
        ))
    }) {
        Ok((items, mutes)) => {
            axum::Json(serde_json::json!({"items":items,"mutes":mutes})).into_response()
        }
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_store_unavailable",
        ),
    }
}

async fn inspect(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((bridge, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(error) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return error;
    }
    let data_dir = state.config.read().data_dir.clone();
    match BridgeOutbox::shared(&data_dir).and_then(|outbox| outbox.inspect(&bridge, &id)) {
        Ok(Some(item)) => axum::Json(item).into_response(),
        Ok(None) => failure(StatusCode::NOT_FOUND, "attention_candidate_not_found"),
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "attention_store_unavailable",
        ),
    }
}

async fn act(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((bridge, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(error) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return error;
    }
    let Ok(action) = serde_json::from_slice::<Action>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "attention_invalid_action");
    };
    let token = crate::operator_auth::extract_bearer(&headers).unwrap_or("");
    let owner = zeroclaw_runtime::security::pairing::PairingGuard::token_hash(token);
    let data_dir = state.config.read().data_dir.clone();
    let outbox = match BridgeOutbox::shared(&data_dir) {
        Ok(outbox) => outbox,
        Err(_) => {
            return failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "attention_store_unavailable",
            );
        }
    };
    match outbox.owner_action(
        &bridge,
        &action.recipient,
        &action.source_kind,
        &action.source_id,
        &id,
        &action.action,
        action.until,
        &owner,
    ) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => failure(StatusCode::NOT_FOUND, "attention_candidate_not_found"),
        Err(_) => failure(StatusCode::CONFLICT, "attention_action_not_applied"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use zeroclaw_config::pairing::PairingGuard;

    async fn request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let wire = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(wire.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (
            head.split_whitespace().nth(1).unwrap().parse().unwrap(),
            body.into(),
        )
    }

    #[tokio::test]
    async fn http_attention_lists_exact_candidates_and_requires_owner_actions_without_pairing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = crate::tests::admin_paircode_state(&tmp, false, false);
        state.pairing = Arc::new(PairingGuard::new(
            false,
            &[PairingGuard::token_hash("owner-token")],
        ));
        state.config.write().gateway.bridges.insert(
            "tg".into(),
            zeroclaw_config::schema::GatewayBridgeConfig {
                token_hash: PairingGuard::token_hash("bridge-token"),
                ..Default::default()
            },
        );
        let store = BridgeOutbox::shared(&state.config.read().data_dir).unwrap();
        let id = store
            .enqueue_source("tg", "42", None, "private notice", "cron", "job", "run")
            .unwrap();
        // Same live-server harness and connect-info path as the gateway's WS
        // tests, with the production route builder and HTTP body-limit layer.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = routes()
            .with_state(state)
            .layer(tower_http::limit::RequestBodyLimitLayer::new(
                crate::MAX_BODY_SIZE,
            ));
        let server = zeroclaw_spawn::spawn!(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let path = format!("/api/attention/tg/{id}");
        let action = serde_json::json!({"recipient":"42","source_kind":"cron","source_id":"job","action":"dismiss"}).to_string();
        for token in [None, Some("bridge-token")] {
            assert_eq!(
                request(addr, "GET", "/api/attention/tg", token, "").await.0,
                401
            );
            assert_eq!(request(addr, "GET", &path, token, "").await.0, 401);
            assert_eq!(request(addr, "POST", &path, token, &action).await.0, 401);
        }
        let owner = Some("owner-token");
        let (status, body) = request(addr, "GET", "/api/attention/tg?after_seq=0", owner, "").await;
        assert_eq!(status, 200);
        let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed["items"][0]["id"], id);
        assert_eq!(listed["items"][0]["delivery_state"], "accepted");
        assert!(listed["items"][0].get("content").is_none());
        let (status, body) = request(addr, "GET", &path, owner, "").await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["source_id"],
            "job"
        );
        let wrong = serde_json::json!({"recipient":"42","source_kind":"cron","source_id":"another-job","action":"dismiss"}).to_string();
        assert_eq!(request(addr, "POST", &path, owner, &wrong).await.0, 404);
        assert_eq!(
            store.inspect("tg", &id).unwrap().unwrap()["delivery_state"],
            "accepted"
        );
        assert_eq!(request(addr, "POST", &path, owner, &action).await.0, 204);
        assert_eq!(
            store.inspect("tg", &id).unwrap().unwrap()["delivery_state"],
            "dismissed"
        );
        server.abort();
    }
}
