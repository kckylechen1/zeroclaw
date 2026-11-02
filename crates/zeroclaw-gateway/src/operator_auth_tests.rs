//! HTTP contract matrix for the existing operator-protected handlers.
use super::*;
#[cfg(feature = "nodes")]
use crate::api_node_identity;
use crate::{api_backup_retention, api_soul, api_user_model};
#[cfg(feature = "nodes")]
use axum::routing::delete;
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request},
    routing::{get, post, put},
};
use std::sync::Arc;
use tower::ServiceExt;

fn state_with_operator(configured: bool, require_pairing: bool) -> AppState {
    // Deserialize the wire config so the pre-fix source can run the same red contract.
    let mut wire = serde_json::to_value(zeroclaw_config::schema::Config::default()).unwrap();
    if configured {
        wire["gateway"]["operator"] =
            serde_json::json!({"token_hashes": [PairingGuard::token_hash("operator-fixture")]});
    }
    let mut state = crate::api::test_state(serde_json::from_value(wire).unwrap());
    state.pairing = Arc::new(PairingGuard::new(
        require_pairing,
        &[PairingGuard::token_hash("paired-fixture")],
    ));
    state
}

fn router(state: AppState) -> Router {
    let router = Router::new()
        .merge(crate::api_review::routes())
        .merge(crate::api_attention::routes())
        // ── User Model operator review surface ──
        .route("/api/user-model/candidates", get(api_user_model::list_candidates))
        .route("/api/user-model/candidates/{id}", get(api_user_model::candidate_history))
        .route("/api/user-model/heads", get(api_user_model::list_heads))
        .route(
            "/api/user-model/candidates/{id}/review",
            post(api_user_model::review_candidate),
        )
        .route("/api/user-model/statements", post(api_user_model::create_statement))
        // ── Governed Soul owner surface (ADR-015) ──
        .route("/api/soul", get(api_soul::get_soul))
        .route("/api/soul/history", get(api_soul::get_history))
        .route("/api/soul/export", get(api_soul::get_export))
        .route("/api/soul/identity", put(api_soul::put_identity))
        .route("/api/soul/principles", put(api_soul::put_principles))
        .route("/api/soul/rollback", post(api_soul::post_rollback))
        .route("/api/soul/drift", get(api_soul::get_drift))
        .route("/api/soul/anchor", post(api_soul::post_anchor))
        .route(
            "/api/soul/anchor/rollback",
            post(api_soul::post_anchor_rollback),
        )
        .route("/api/soul/story", put(api_soul::put_story))
        .route("/api/soul/tone", put(api_soul::put_tone))
        .route(
            "/api/soul/tone/exemplars",
            put(api_soul::put_tone_exemplars),
        )
        .route("/api/soul/proposals", get(api_soul::get_proposals))
        .route(
            "/api/soul/proposals/{id}/resolve",
            post(api_soul::post_resolve_proposal),
        )
        // ── Backup / data-retention operator surface ──
        // Thin operator-bearer-gated entries over the same BackupTool /
        // DataManagementTool command methods the model-visible tools use;
        // restore keeps its confirm/dry-run guard, purge keeps its
        // dry-run default. Route table lives with the handlers.
        .merge(api_backup_retention::routes());
    #[cfg(feature = "nodes")]
    let router = router
        .route(
            "/api/node-identities/pairing",
            post(api_node_identity::issue_pairing),
        )
        .route(
            "/api/node-identities",
            post(api_node_identity::enroll_identity),
        )
        .route(
            "/api/node-identities/{id}",
            delete(api_node_identity::revoke_identity),
        );
    router.with_state(state)
}

fn operations() -> Vec<(Method, &'static str)> {
    vec![
        (Method::GET, "/api/review/inbox"),
        (Method::GET, "/api/attention/missing"),
        (Method::GET, "/api/attention/missing/missing"),
        (Method::POST, "/api/attention/missing/missing"),
        (Method::GET, "/api/user-model/candidates"),
        (Method::GET, "/api/user-model/candidates/missing"),
        (Method::GET, "/api/user-model/heads"),
        (Method::POST, "/api/user-model/candidates/missing/review"),
        (Method::POST, "/api/user-model/statements"),
        (Method::GET, "/api/soul"),
        (Method::GET, "/api/soul/history"),
        (Method::GET, "/api/soul/export"),
        (Method::PUT, "/api/soul/identity"),
        (Method::PUT, "/api/soul/principles"),
        (Method::POST, "/api/soul/rollback"),
        (Method::GET, "/api/soul/drift"),
        (Method::POST, "/api/soul/anchor"),
        (Method::POST, "/api/soul/anchor/rollback"),
        (Method::PUT, "/api/soul/story"),
        (Method::PUT, "/api/soul/tone"),
        (Method::PUT, "/api/soul/tone/exemplars"),
        (Method::GET, "/api/soul/proposals"),
        (Method::POST, "/api/soul/proposals/1/resolve"),
        (Method::POST, "/api/node-identities/pairing"),
        (Method::POST, "/api/node-identities"),
        (Method::DELETE, "/api/node-identities/missing"),
        (Method::GET, "/api/agents/missing/backup"),
        (Method::POST, "/api/agents/missing/backup"),
        (Method::POST, "/api/agents/missing/backup/missing/verify"),
        (Method::POST, "/api/agents/missing/backup/missing/restore"),
        (Method::GET, "/api/agents/missing/data-retention/status"),
        (Method::GET, "/api/agents/missing/data-retention/stats"),
        (Method::POST, "/api/agents/missing/data-retention/purge"),
    ]
    .into_iter()
    .filter(|(_, path)| cfg!(feature = "nodes") || !path.starts_with("/api/node-identities"))
    .collect()
}

async fn status(state: AppState, method: Method, path: &str, token: Option<&str>) -> StatusCode {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut config = state.config.write();
        config.data_dir = tmp.path().join("data");
        config.config_path = tmp.path().join("config.toml");
    }
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut request = request.body(Body::from("{}")).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
    router(state).oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn operator_credential_http_matrix() {
    for require_pairing in [true, false] {
        for (method, path) in operations() {
            let paired = status(
                state_with_operator(true, require_pairing),
                method.clone(),
                path,
                Some("paired-fixture"),
            )
            .await;
            assert_eq!(
                paired,
                StatusCode::FORBIDDEN,
                "{method} {path}, pairing={require_pairing}"
            );
            let operator = status(
                state_with_operator(true, require_pairing),
                method.clone(),
                path,
                Some("operator-fixture"),
            )
            .await;
            let legacy = status(
                state_with_operator(false, require_pairing),
                method.clone(),
                path,
                Some("paired-fixture"),
            )
            .await;
            assert!(
                ![
                    StatusCode::UNAUTHORIZED,
                    StatusCode::FORBIDDEN,
                    StatusCode::TOO_MANY_REQUESTS
                ]
                .contains(&operator),
                "operator blocked: {method} {path}: {operator}"
            );
            assert_eq!(
                operator, legacy,
                "operator must reach the same handler behavior: {method} {path}"
            );
            let anonymous = status(
                state_with_operator(true, require_pairing),
                method.clone(),
                path,
                None,
            )
            .await;
            assert_eq!(anonymous, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }
}

fn headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

#[test]
fn operator_gate_empty_invalid_rotation_and_separation() {
    let state = state_with_operator(true, true);
    assert!(require_operator(&state, &headers("operator-fixture")).is_ok());
    assert!(crate::api::require_auth(&state, &headers("operator-fixture")).is_err());
    assert!(crate::api::require_auth(&state, &headers("paired-fixture")).is_ok());
    for token in ["", "unknown", "bridge-fixture", "node-fixture"] {
        assert_eq!(
            require_operator(&state, &headers(token)),
            Err((StatusCode::UNAUTHORIZED, "Unauthorized"))
        );
    }
    state.config.write().gateway.operator.token_hashes = vec!["malformed".into()];
    assert_eq!(
        require_operator(&state, &headers("paired-fixture")),
        Err((StatusCode::FORBIDDEN, "Forbidden"))
    );
    state.config.write().gateway.operator.token_hashes = vec![PairingGuard::token_hash("rotated")];
    assert!(require_operator(&state, &headers("operator-fixture")).is_err());
    assert!(require_operator(&state, &headers("rotated")).is_ok());
    state.config.write().gateway.operator.token_hashes.clear();
    assert!(require_operator(&state, &headers("paired-fixture")).is_ok());
    assert!(require_operator(&state, &headers("rotated")).is_err());
}

#[test]
fn rejected_paired_operator_requests_keep_auth_rate_limit() {
    let state = state_with_operator(true, false);
    let peer = SocketAddr::from(([127, 0, 0, 2], 12345));
    let headers = headers("paired-fixture");
    let mut limited = false;
    for _ in 0..100 {
        let status = gate_operator_identity(&state, peer, &headers)
            .unwrap()
            .status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert!(limited);
}

#[tokio::test]
async fn operator_credentials_successful_reads() {
    for (method, path) in [
        (Method::GET, "/api/user-model/candidates"),
        (Method::GET, "/api/soul?agent=owner"),
        (Method::GET, "/api/agents/owner/backup"),
    ] {
        let state = state_with_operator(true, true);
        state
            .config
            .write()
            .agents
            .insert("owner".into(), Default::default());
        assert_eq!(
            status(state, method, path, Some("operator-fixture")).await,
            StatusCode::OK
        );
    }
    #[cfg(feature = "nodes")]
    assert_eq!(
        status(
            state_with_operator(true, true),
            Method::POST,
            "/api/node-identities/pairing",
            Some("operator-fixture")
        )
        .await,
        StatusCode::OK
    );
}

#[test]
fn legacy_operator_mode_emits_migration_warning() {
    let _writer = zeroclaw_log::__private_test_writer_lock();
    let _hook = zeroclaw_log::__private_test_hook_lock();
    zeroclaw_log::try_install_capture_subscriber();
    let mut rx = zeroclaw_log::subscribe_or_install();
    warn_legacy_operator(&Default::default());
    let mut found = false;
    while let Ok(event) = rx.try_recv() {
        if event["severity_text"] == "WARN"
            && event["message"]
                .as_str()
                .is_some_and(|message| message.contains("gateway.operator.token_hashes"))
        {
            found = true;
        }
    }
    assert!(
        found,
        "legacy authority must warn with the migration config key"
    );
}
