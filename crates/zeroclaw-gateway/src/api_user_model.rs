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
use axum::http::{HeaderMap, StatusCode, Uri};
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

#[derive(Default, serde::Deserialize)]
struct CandidateQuery {
    #[serde(default)]
    pending: bool,
}

/// GET /api/user-model/candidates — all history by default; pending=true
/// selects candidates without a committed review receipt.
pub async fn list_candidates(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let query = match axum::extract::Query::<CandidateQuery>::try_from_uri(&uri) {
        Ok(query) => query.0,
        Err(error) => return error_json(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let data_dir = data_dir_of(&state);
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir)?;
        if query.pending {
            store.list_pending_candidates()
        } else {
            store.list_candidates()
        }
        .map_err(|err| err.to_string())
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

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CandidateReviewState {
    Pending,
    Accepted,
    Rejected,
    Narrowed,
    Superseded,
}

impl From<Option<ReviewAction>> for CandidateReviewState {
    fn from(action: Option<ReviewAction>) -> Self {
        match action {
            None => Self::Pending,
            Some(ReviewAction::Accept) => Self::Accepted,
            Some(ReviewAction::Reject) => Self::Rejected,
            Some(ReviewAction::Narrow) => Self::Narrowed,
            Some(ReviewAction::Supersede) => Self::Superseded,
        }
    }
}

/// GET /api/user-model/candidates/{id} — candidate evidence and committed
/// review history, including the latest derived review state.
pub async fn candidate_history(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(candidate_id): Path<String>,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let data_dir = data_dir_of(&state);
    let result = tokio::task::spawn_blocking(move || {
        let store = cached_store(&data_dir)?;
        store
            .candidate_history(&candidate_id)
            .map_err(|err| err.to_string())
    })
    .await;
    match result {
        Ok(Ok(Some((candidate, review_receipts)))) => {
            let review_state =
                CandidateReviewState::from(review_receipts.last().map(|receipt| receipt.action));
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "candidate": candidate,
                    "review_state": review_state,
                    "review_receipts": review_receipts,
                })),
            )
                .into_response()
        }
        Ok(Ok(None)) => error_json(StatusCode::NOT_FOUND, "unknown candidate id"),
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

    async fn history_http(
        state: &AppState,
        candidate_id: &str,
        headers: HeaderMap,
    ) -> (StatusCode, serde_json::Value) {
        json_of(
            candidate_history(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                headers,
                Path(candidate_id.to_string()),
            )
            .await,
        )
        .await
    }

    #[tokio::test]
    async fn committed_review_history_survives_store_reopen_without_writes() {
        let (dir, state) = state_with_tempdir();
        let store = UserModelStore::open(dir.path()).unwrap();
        let pending = store
            .record_observation(UserModelKind::Habit, "pending", "pending.key", "[1]", 100)
            .unwrap();
        let mut reviewed = Vec::new();
        for (index, action, name, expected_state) in [
            (0, ReviewAction::Accept, "accepted", "accepted"),
            (1, ReviewAction::Reject, "rejected", "rejected"),
            (2, ReviewAction::Narrow, "narrowed", "narrowed"),
            (3, ReviewAction::Supersede, "superseded", "superseded"),
        ] {
            let candidate = store
                .record_observation(UserModelKind::Habit, name, name, "[2]", 110 + index)
                .unwrap();
            let receipt = store
                .review_candidate(
                    &candidate.id,
                    action,
                    "operator",
                    Some("review note"),
                    (action == ReviewAction::Narrow).then_some("session:fixture"),
                    200 + index,
                )
                .unwrap();
            reviewed.push((candidate, receipt, expected_state));
        }
        let legacy = store
            .record_observation(UserModelKind::Habit, "legacy", "legacy.key", "[]", 300)
            .unwrap();
        let first = store
            .review_candidate(
                &legacy.id,
                ReviewAction::Reject,
                "operator",
                None,
                None,
                301,
            )
            .unwrap();
        drop(store);
        let fixture = rusqlite::Connection::open(dir.path().join("user_model.db")).unwrap();
        fixture
            .execute(
                "INSERT INTO user_model_review_receipts
                 (id, candidate_id, action, reviewer, note, at_unix)
                 VALUES (?1, ?2, 'reject', 'operator', NULL, 302)",
                rusqlite::params!["legacy-later", legacy.id],
            )
            .unwrap();
        drop(fixture);
        let before = review_scope_rows(dir.path());
        let (status, detail) = history_http(&state, &pending.id, operator_headers()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["candidate"]["evidence"], "[1]");
        assert_eq!(detail["review_state"], "pending");
        assert_eq!(detail["review_receipts"], serde_json::json!([]));
        for (candidate, receipt, expected_state) in reviewed {
            let (status, detail) = history_http(&state, &candidate.id, operator_headers()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(detail["candidate"]["id"], candidate.id);
            assert_eq!(detail["review_state"], expected_state);
            assert_eq!(detail["review_receipts"].as_array().unwrap().len(), 1);
            assert_eq!(detail["review_receipts"][0]["id"], receipt.id);
            assert_eq!(
                detail["review_receipts"][0]["action"],
                serde_json::to_value(receipt.action).unwrap()
            );
            assert_eq!(detail["review_receipts"][0]["note"], "review note");
        }
        let (status, detail) = history_http(&state, &legacy.id, operator_headers()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["review_state"], "rejected");
        assert_eq!(detail["review_receipts"].as_array().unwrap().len(), 2);
        assert_eq!(detail["review_receipts"][0]["id"], first.id);
        assert_eq!(detail["review_receipts"][1]["id"], "legacy-later");
        assert_eq!(review_scope_rows(dir.path()), before);
    }

    #[tokio::test]
    async fn history_http_reports_last_inserted_review_with_same_second_reversed_ids() {
        let (dir, state) = state_with_tempdir();
        let store = UserModelStore::open(dir.path()).unwrap();
        let candidate = store
            .record_observation(UserModelKind::Habit, "habit", "habit.key", "[]", 100)
            .unwrap();
        let fixture = rusqlite::Connection::open(dir.path().join("user_model.db")).unwrap();
        fixture
            .execute(
                "INSERT INTO user_model_review_receipts
                 (id, candidate_id, action, reviewer, note, at_unix)
                 VALUES ('zzzz-first', ?1, 'reject', 'operator', NULL, 200)",
                rusqlite::params![candidate.id],
            )
            .unwrap();
        drop(fixture);
        let narrowed = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "operator",
                None,
                Some("session:fixture"),
                200,
            )
            .unwrap();
        assert!(narrowed.id.as_str() < "zzzz-first");
        drop(store);

        let before = review_scope_rows(dir.path());
        let (status, detail) = history_http(&state, &candidate.id, operator_headers()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["review_state"], "narrowed");
        assert_eq!(detail["review_receipts"][0]["id"], "zzzz-first");
        assert_eq!(detail["review_receipts"][1]["id"], narrowed.id);
        assert_eq!(review_scope_rows(dir.path()), before);
    }

    #[tokio::test]
    async fn candidate_history_authorizes_before_store_and_unknown_is_not_found() {
        let (dir, state) = state_with_tempdir();
        let (status, _) = history_http(&state, "absent", anon_headers()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!dir.path().join("user_model.db").exists());
        assert!(!store_handles().lock().contains_key(dir.path()));
        let (status, detail) = history_http(&state, "absent", operator_headers()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(detail["error"], "unknown candidate id");
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
                Uri::from_static("/api/user-model/candidates"),
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
    fn review_scope_rows(data_dir: &std::path::Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let conn = rusqlite::Connection::open_with_flags(
            data_dir.join("user_model.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "user_model_candidates",
            "user_model_review_receipts",
            "user_model_revisions",
        ]
        .iter()
        .map(|table| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
                .unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .collect()
    }

    #[tokio::test]
    async fn invalid_narrow_scope_over_http_leaves_no_receipt() {
        let (dir, state) = state_with_tempdir();
        let store = cached_store(&dir.path().to_path_buf()).unwrap();
        let candidate = store
            .record_observation(
                UserModelKind::Habit,
                "private observation",
                "private.scope",
                "[]",
                100,
            )
            .unwrap();
        let before = review_scope_rows(dir.path());
        let (status, response) = json_of(
            review_candidate(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(candidate.id.clone()),
                body(&serde_json::json!({"action":"narrow","narrowed_scope":"task:unsupported"})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (pending_status, pending) = pending_http(
            &state,
            "/api/user-model/candidates?pending=true",
            operator_headers(),
        )
        .await;
        assert_eq!(pending_status, StatusCode::OK);
        assert_eq!(pending["candidates"].as_array().unwrap().len(), 1);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("invalid narrowed scope")
        );
        assert_eq!(review_scope_rows(dir.path()), before);
        let (status, response) = json_of(
            review_candidate(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(candidate.id),
                body(&serde_json::json!({"action":"narrow","narrowed_scope":"session:A"})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["receipt"]["action"], "narrow");
        let after = review_scope_rows(dir.path());
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].len(), 1);
        assert_eq!(after[2].len(), 1);
        let heads = store.active_heads(None).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].scope, "session:A");
        assert_eq!(
            serde_json::to_value(&heads[0]).unwrap()["authority"],
            "owner_ratified"
        );
    }
    #[tokio::test]
    async fn review_revision_insert_failure_over_http_rolls_back_receipt() {
        let (dir, state) = state_with_tempdir();
        let store = cached_store(&dir.path().to_path_buf()).unwrap();
        let candidate = store
            .record_observation(UserModelKind::Habit, "private", "atomic.http", "[]", 100)
            .unwrap();
        // Persistent only inside this disposable fixture DB: the handler's
        // canonical connection must see it; TEMP would be connection-local.
        let fixture = rusqlite::Connection::open(dir.path().join("user_model.db")).unwrap();
        fixture.execute_batch("CREATE TRIGGER private_http_revision_fault BEFORE INSERT ON user_model_revisions BEGIN SELECT RAISE(ABORT, 'private HTTP revision fault'); END;").unwrap();
        let before = review_scope_rows(dir.path());
        let (status, response) = json_of(
            review_candidate(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(candidate.id.clone()),
                body(&serde_json::json!({"action":"accept"})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (pending_status, pending) = pending_http(
            &state,
            "/api/user-model/candidates?pending=true",
            operator_headers(),
        )
        .await;
        assert_eq!(pending_status, StatusCode::OK);
        assert_eq!(pending["candidates"].as_array().unwrap().len(), 1);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("private HTTP revision fault")
        );
        assert_eq!(review_scope_rows(dir.path()), before);
        fixture
            .execute_batch("DROP TRIGGER private_http_revision_fault;")
            .unwrap();
        let (status, _) = json_of(
            review_candidate(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(candidate.id),
                body(&serde_json::json!({"action":"accept"})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let after = review_scope_rows(dir.path());
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].len(), 1);
        assert_eq!(after[2].len(), 1);
        assert_eq!(
            serde_json::to_value(&store.active_heads(None).unwrap()[0]).unwrap()["authority"],
            "owner_ratified"
        );
    }
    async fn pending_http(
        state: &AppState,
        uri: &str,
        headers: HeaderMap,
    ) -> (StatusCode, serde_json::Value) {
        json_of(
            list_candidates(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                headers,
                uri.parse().unwrap(),
            )
            .await,
        )
        .await
    }

    #[tokio::test]
    async fn pending_query_uri_preserves_history_and_review_authority() {
        let (dir, state) = state_with_tempdir();
        let (status, empty) = pending_http(
            &state,
            "/api/user-model/candidates?pending=true",
            operator_headers(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(empty["candidates"], serde_json::json!([]));
        let store = cached_store(&dir.path().to_path_buf()).unwrap();
        let c = store
            .record_observation(UserModelKind::Habit, "C", "c", "[]", 100)
            .unwrap();
        let d = store
            .record_observation(UserModelKind::Habit, "D", "d", "[]", 101)
            .unwrap();
        let (_, both) = pending_http(
            &state,
            "/api/user-model/candidates?pending=true",
            operator_headers(),
        )
        .await;
        assert_eq!(both["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(both["candidates"][0]["id"], d.id);
        assert_eq!(both["candidates"][1]["id"], c.id);
        for (candidate, action, remaining) in [(&c, "reject", 1), (&d, "accept", 0)] {
            let (status, _) = json_of(
                review_candidate(
                    State(state.clone()),
                    ConnectInfo(loopback_peer()),
                    operator_headers(),
                    Path(candidate.id.clone()),
                    body(&serde_json::json!({"action":action})),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let (status, pending) = pending_http(
                &state,
                "/api/user-model/candidates?pending=true",
                operator_headers(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(pending["candidates"].as_array().unwrap().len(), remaining);
            if action == "reject" {
                assert_eq!(pending["candidates"][0]["id"], d.id);
            } else {
                assert_eq!(pending["candidates"], serde_json::json!([]));
            }
        }
        for uri in [
            "/api/user-model/candidates",
            "/api/user-model/candidates?pending=false",
        ] {
            let (status, history) = pending_http(&state, uri, operator_headers()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(history["candidates"].as_array().unwrap().len(), 2);
            assert_eq!(history["candidates"][0]["id"], d.id);
            assert_eq!(history["candidates"][1]["id"], c.id);
        }
        let e = store
            .record_observation(UserModelKind::Habit, "E", "e", "[]", 202)
            .unwrap();
        let before = review_scope_rows(dir.path());
        let (_, pending) = pending_http(
            &state,
            "/api/user-model/candidates?pending=true",
            operator_headers(),
        )
        .await;
        assert_eq!(pending["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(pending["candidates"][0]["id"], e.id);
        pending_http(&state, "/api/user-model/candidates", operator_headers()).await;
        assert_eq!(review_scope_rows(dir.path()), before);
        assert_eq!(
            serde_json::to_value(&store.active_heads(None).unwrap()[0]).unwrap()["authority"],
            "owner_ratified"
        );
    }

    #[tokio::test]
    async fn pending_query_rejects_malformed_after_auth_without_store_open() {
        let (dir, state) = state_with_tempdir();
        for query in [
            "pending=",
            "pending=0",
            "pending=invalid",
            "pending=true&pending=false",
        ] {
            let uri = format!("/api/user-model/candidates?{query}");
            let (status, _) = pending_http(&state, &uri, anon_headers()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            let (status, _) = pending_http(&state, &uri, operator_headers()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!dir.path().join("user_model.db").exists());
            assert!(!store_handles().lock().contains_key(dir.path()));
        }
    }
}
