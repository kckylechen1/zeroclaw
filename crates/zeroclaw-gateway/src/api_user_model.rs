//! `GET/POST /api/user-model/*` — operator review surface for the User
//! Model authority. Every route is operator-gated: reviewing and
//! authoring is owner authority, exactly the surface the store's rules
//! exist to govern.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use zeroclaw_memory::companion::{
    ReviewAction, UserModelKind, UserModelReviewReceipt, UserModelRevision, UserModelStore,
};

use crate::AppState;

/// One open store handle per data_dir. The sqlite file is the source of
/// truth; these are views (WAL + busy_timeout support multi-connection).
fn store_handles() -> &'static Mutex<HashMap<PathBuf, Arc<UserModelStore>>> {
    static HANDLES: OnceLock<Mutex<HashMap<PathBuf, Arc<UserModelStore>>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_store(data_dir: &PathBuf) -> Result<Arc<UserModelStore>, String> {
    let mut handles = store_handles().lock();
    if let Some(store) = handles.get(data_dir) {
        return Ok(Arc::clone(store));
    }
    let store = Arc::new(UserModelStore::open(data_dir).map_err(|err| err.to_string())?);
    handles.insert(data_dir.clone(), Arc::clone(&store));
    Ok(store)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn error_json(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

fn data_dir_of(state: &AppState) -> PathBuf {
    state.config.read().data_dir.clone()
}

/// GET /api/user-model/candidates — pending observations with evidence.
pub async fn list_candidates(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let data_dir = data_dir_of(&state);
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir)?;
        store.list_candidates().map_err(|err| err.to_string())
    })
    .await;
    match result {
        Ok(Ok(candidates)) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "candidates": candidates })),
        )
            .into_response(),
        Ok(Err(err)) => error_json(StatusCode::SERVICE_UNAVAILABLE, &err),
        Err(_) => error_json(StatusCode::INTERNAL_SERVER_ERROR, "store task failed"),
    }
}

/// GET /api/user-model/heads — active, applicable revisions right now.
pub async fn list_heads(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let data_dir = data_dir_of(&state);
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir)?;
        store.active_heads(None).map_err(|err| err.to_string())
    })
    .await;
    match result {
        Ok(Ok(heads)) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "heads": heads })),
        )
            .into_response(),
        Ok(Err(err)) => error_json(StatusCode::SERVICE_UNAVAILABLE, &err),
        Err(_) => error_json(StatusCode::INTERNAL_SERVER_ERROR, "store task failed"),
    }
}

#[derive(serde::Deserialize)]
struct ReviewBody {
    action: String,
    note: Option<String>,
    narrowed_scope: Option<String>,
}

/// POST /api/user-model/candidates/{id}/review — explicit owner action on
/// a candidate. `narrow` requires `narrowed_scope`.
pub async fn review_candidate(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(candidate_id): Path<String>,
    body: Bytes,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let body = match serde_json::from_slice::<ReviewBody>(&body) {
        Ok(body) => body,
        Err(err) => return error_json(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let action = match body.action.as_str() {
        "accept" => ReviewAction::Accept,
        "reject" => ReviewAction::Reject,
        "narrow" => ReviewAction::Narrow,
        "supersede" => ReviewAction::Supersede,
        other => {
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("unknown action '{other}': accept | reject | narrow | supersede"),
            );
        }
    };
    if action == ReviewAction::Narrow && body.narrowed_scope.as_deref().is_none_or(str::is_empty) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "narrow requires a non-empty narrowed_scope",
        );
    }
    let data_dir = data_dir_of(&state);
    let candidate = candidate_id.clone();
    let note = body.note.clone();
    let narrowed = body.narrowed_scope.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir)?;
        store
            .review_candidate(
                &candidate,
                action,
                "operator",
                note.as_deref(),
                narrowed.as_deref(),
                now_unix(),
            )
            .map_err(|err| err.to_string())
    })
    .await;
    match result {
        Ok(Ok(receipt)) => review_response(action, &receipt),
        Ok(Err(err)) if err.contains("no rows") => {
            error_json(StatusCode::NOT_FOUND, "unknown candidate id")
        }
        Ok(Err(err)) => error_json(StatusCode::SERVICE_UNAVAILABLE, &err),
        Err(_) => error_json(StatusCode::INTERNAL_SERVER_ERROR, "store task failed"),
    }
}

fn review_response(action: ReviewAction, receipt: &UserModelReviewReceipt) -> Response {
    let mut body = serde_json::json!({ "receipt": receipt });
    if action != ReviewAction::Reject {
        body["note_to_reviewer"] =
            serde_json::json!("re-read /api/user-model/heads for the resulting revision");
    }
    (StatusCode::OK, axum::Json(body)).into_response()
}

#[derive(serde::Deserialize)]
struct StatementBody {
    kind: String,
    statement: String,
    semantic_key: String,
    scope: Option<String>,
}

/// POST /api/user-model/statements — record an explicit owner-authored
/// statement; it becomes the active revision for its key immediately.
pub async fn create_statement(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let body = match serde_json::from_slice::<StatementBody>(&body) {
        Ok(body) => body,
        Err(err) => return error_json(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let kind = match body.kind.as_str() {
        "value" => UserModelKind::Value,
        "goal" => UserModelKind::Goal,
        "preference" => UserModelKind::Preference,
        "habit" => UserModelKind::Habit,
        "constraint" => UserModelKind::Constraint,
        other => {
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("unknown kind '{other}': value | goal | preference | habit | constraint"),
            );
        }
    };
    if body.statement.trim().is_empty() || body.semantic_key.trim().is_empty() {
        return error_json(
            StatusCode::BAD_REQUEST,
            "statement and semantic_key are required",
        );
    }
    let data_dir = data_dir_of(&state);
    let statement = body.statement;
    let semantic_key = body.semantic_key;
    let scope = body.scope.unwrap_or_else(|| "global".to_string());
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir)?;
        store
            .record_owner_statement(kind, &statement, &semantic_key, &scope, now_unix())
            .map_err(|err| err.to_string())
    })
    .await;
    match result {
        Ok(Ok(revision)) => statement_response(&revision),
        Ok(Err(err)) => error_json(StatusCode::SERVICE_UNAVAILABLE, &err),
        Err(_) => error_json(StatusCode::INTERNAL_SERVER_ERROR, "store task failed"),
    }
}

fn statement_response(revision: &UserModelRevision) -> Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "revision": revision })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use http_body_util::BodyExt;

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9))
    }

    fn operator_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str("Bearer op-token").unwrap(),
        );
        headers
    }

    fn anon_headers() -> HeaderMap {
        HeaderMap::new()
    }

    fn state_with_tempdir() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = zeroclaw_config::schema::Config::default();
        config.nodes.enabled = true;
        config.nodes.auth_token = Some("secret".into());
        config.data_dir = dir.path().to_path_buf();
        let mut state = crate::api::test_state(config);
        state.pairing = Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
            true,
            &["op-token".into()],
        ));
        (dir, state)
    }

    async fn json_of(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }));
        (status, json)
    }

    fn body<T: serde::Serialize>(value: &T) -> Bytes {
        Bytes::from(serde_json::to_vec(value).unwrap())
    }

    #[tokio::test]
    async fn statement_review_and_heads_roundtrip_over_http() {
        let (_dir, state) = state_with_tempdir();

        // Anonymous access is rejected before touching the store.
        let (status, _) = json_of(
            list_heads(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                anon_headers(),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Owner statement becomes active immediately.
        let (status, created) = json_of(
            create_statement(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                body(&serde_json::json!({
                    "kind": "preference",
                    "statement": "Always give me the engineering conclusion first.",
                    "semantic_key": "communication.conclusion-first",
                })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "statement body={created}");
        assert_eq!(
            created["revision"]["authority"], "owner_authored",
            "the HTTP surface must not create non-owner authority classes"
        );

        // An observation candidate shows up for review, never as a head.
        let seed_dir = state.config.read().data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let store = cached_store(&seed_dir).expect("seed store");
            store
                .record_observation(
                    UserModelKind::Habit,
                    "User keeps reformatting tables manually.",
                    "formatting.tables",
                    "[]",
                    now_unix(),
                )
                .expect("seed candidate");
        })
        .await
        .unwrap();
        let (status, listed) = json_of(
            list_candidates(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let candidates = listed["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate_id = candidates[0]["id"].as_str().unwrap().to_string();

        // Reject keeps the candidate but never activates anything.
        let (status, _) = json_of(
            review_candidate(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(candidate_id.clone()),
                body(&serde_json::json!({ "action": "reject", "note": "not a habit" })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, heads) = json_of(
            list_heads(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            heads["heads"].as_array().unwrap().len(),
            1,
            "only the owner statement may be active"
        );

        // Unknown candidate is a 404, not a 500.
        let (status, _) = json_of(
            review_candidate(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("missing".to_string()),
                body(&serde_json::json!({ "action": "accept" })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Narrow without a scope is a 400.
        let (status, _) = json_of(
            review_candidate(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(candidate_id),
                body(&serde_json::json!({ "action": "narrow" })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
