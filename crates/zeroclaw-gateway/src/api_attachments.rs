//! Ephemeral HTTP payloads. The hub creates and owns these bytes; request IDs
//! and history remain owned by their existing conversation/session services.
use super::AppState;
use axum::{
    Router,
    body::{Bytes, to_bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_STORE_BYTES: usize = 32 * 1024 * 1024;
const MAX_STORE_ITEMS: usize = 128;
const MAX_SCOPE_ITEMS: usize = 16;
pub(crate) const MAX_MESSAGE_ITEMS: usize = 4;
const TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    pub subject: String,
    pub session: String,
    pub agent: String,
}
#[derive(Clone)]
struct Attachment {
    scope: Scope,
    name: String,
    mime: String,
    data: Bytes,
    expires: Instant,
}
pub(crate) struct Store {
    items: Mutex<HashMap<String, Attachment>>,
    uploads: tokio::sync::Semaphore,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            items: Mutex::default(),
            uploads: tokio::sync::Semaphore::new(4),
        }
    }
}
impl Store {
    fn prune(items: &mut HashMap<String, Attachment>) {
        items.retain(|_, a| a.expires > Instant::now());
    }
    pub(crate) fn insert(
        &self,
        scope: Scope,
        name: String,
        mime: String,
        data: Bytes,
    ) -> Result<Value, StatusCode> {
        if data.is_empty() || data.len() > MAX_FILE_BYTES {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        if mime.starts_with("text/") || mime == "application/json" {
            if data.len() > MAX_TEXT_BYTES || std::str::from_utf8(&data).is_err() {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
        } else {
            let signature = match mime.as_str() {
                "image/png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
                "image/jpeg" => data.starts_with(b"\xff\xd8\xff"),
                "image/gif" => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
                "image/webp" => data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP"),
                _ => false,
            };
            if !signature {
                return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }
        }
        let mut items = self.items.lock();
        Self::prune(&mut items);
        if items.len() >= MAX_STORE_ITEMS
            || items.values().map(|a| a.data.len()).sum::<usize>() + data.len() > MAX_STORE_BYTES
            || items.values().filter(|a| a.scope == scope).count() >= MAX_SCOPE_ITEMS
        {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let info = json!({"id":id,"file_name":name,"mime_type":mime,"size":data.len(),"expires_in_secs":TTL.as_secs()});
        items.insert(
            id,
            Attachment {
                scope,
                name,
                mime,
                data,
                expires: Instant::now() + TTL,
            },
        );
        Ok(info)
    }
    fn get(&self, scope: &Scope, id: &str) -> Result<Attachment, StatusCode> {
        let mut items = self.items.lock();
        Self::prune(&mut items);
        items
            .get(id)
            .filter(|a| &a.scope == scope)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)
    }
    pub(crate) fn materialize(
        &self,
        scope: &Scope,
        ids: &[String],
        content: &str,
    ) -> Result<String, StatusCode> {
        let mut out = content.to_string();
        // Resolve all handles before submitting any input; no partial turns.
        let values = ids
            .iter()
            .map(|id| self.get(scope, id))
            .collect::<Result<Vec<_>, _>>()?;
        for a in values {
            if a.mime.starts_with("image/") {
                out.push_str(&format!(
                    "\n[IMAGE:{}]",
                    zeroclaw_providers::multimodal::image_data_uri(&a.mime, &a.data)
                ));
            } else {
                let text =
                    std::str::from_utf8(&a.data).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
                out.push_str(&format!(
                    "\n<user_attachment name={:?}>\n{}\n</user_attachment>",
                    a.name,
                    zeroclaw_providers::multimodal::quote_attachment_media_markers(text)
                ));
            }
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
pub struct AttachmentQuery {
    pub session_id: String,
    pub agent: String,
    pub file_name: Option<String>,
}
pub(crate) fn scope_authorized(state: &AppState, scope: &Scope) -> bool {
    let config = state.config.read();
    config.agents.get(&scope.agent).is_some_and(|a| a.enabled)
        && (state.pairing.tokens().contains(&scope.subject)
            || config.gateway.bridges.values().any(|b| {
                b.allows_session(&scope.session)
                    && zeroclaw_config::pairing::constant_time_eq(
                        &scope.subject,
                        &b.token_hash.to_ascii_lowercase(),
                    )
            }))
}

fn subject(
    state: &AppState,
    headers: &HeaderMap,
    query: &AttachmentQuery,
) -> Result<Scope, StatusCode> {
    if query.session_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let hash = zeroclaw_config::pairing::PairingGuard::token_hash(token);
    let config = state.config.read();
    if let Some((_, bridge)) = config.gateway.bridge_for_token(token) {
        if !bridge.allows_session(&query.session_id) {
            return Err(StatusCode::FORBIDDEN);
        }
    } else if !state.pairing.tokens().contains(&hash) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !config.agents.get(&query.agent).is_some_and(|a| a.enabled) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Scope {
        subject: hash,
        session: query.session_id.clone(),
        agent: query.agent.clone(),
    })
}
fn failure(status: StatusCode) -> Response {
    (
        status,
        axum::Json(json!({"error":"attachment_request_refused"})),
    )
        .into_response()
}

pub fn routes(state: AppState, timeout_secs: u64) -> Router {
    Router::new()
        .route("/api/attachments", post(upload))
        .route("/api/attachments/{id}", get(fetch))
        .with_state(state)
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(timeout_secs),
        ))
}
async fn upload(
    State(state): State<AppState>,
    Query(query): Query<AttachmentQuery>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let scope = match subject(&state, &headers, &query) {
        Ok(s) => s,
        Err(e) => return failure(e),
    };
    let name = query.file_name.as_deref().unwrap_or("attachment");
    if name.is_empty()
        || name.len() > 128
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '[' | ']' | '<' | '>' | '"'))
        || name == "."
        || name == ".."
    {
        return failure(StatusCode::BAD_REQUEST);
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "text/plain"
            | "text/markdown"
            | "text/csv"
            | "application/json"
            | "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
    ) {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let Ok(_permit) = state.ws_conversations.attachments.uploads.try_acquire() else {
        return failure(StatusCode::TOO_MANY_REQUESTS);
    };
    let bytes = match to_bytes(request.into_body(), MAX_FILE_BYTES).await {
        Ok(b) => b,
        Err(_) => return failure(StatusCode::PAYLOAD_TOO_LARGE),
    };
    // A slow upload must not preserve authority that was revoked meanwhile.
    if subject(&state, &headers, &query).as_ref() != Ok(&scope) {
        return failure(StatusCode::UNAUTHORIZED);
    }
    match state
        .ws_conversations
        .attachments
        .insert(scope, name.to_string(), mime, bytes)
    {
        Ok(info) => (StatusCode::CREATED, axum::Json(info)).into_response(),
        Err(e) => failure(e),
    }
}
async fn fetch(
    State(state): State<AppState>,
    Query(query): Query<AttachmentQuery>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let scope = match subject(&state, &headers, &query) {
        Ok(s) => s,
        Err(e) => return failure(e),
    };
    match state.ws_conversations.attachments.get(&scope, &id) {
        Ok(_) if !scope_authorized(&state, &scope) => failure(StatusCode::UNAUTHORIZED),
        Ok(a) => (
            [
                (header::CONTENT_TYPE, a.mime),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"attachment\"".into(),
                ),
                (header::CACHE_CONTROL, "no-store".into()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            ],
            a.data,
        )
            .into_response(),
        Err(e) => failure(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;
    fn scope() -> Scope {
        Scope {
            subject: "synthetic-owner".into(),
            session: "main".into(),
            agent: "assistant".into(),
        }
    }
    #[test]
    fn scoped_bytes_expire_and_cannot_be_used_across_subject_session_or_agent() {
        let store = Store::default();
        let s = scope();
        let id = store
            .insert(
                s.clone(),
                "note.txt".into(),
                "text/plain".into(),
                Bytes::from_static(b"hi"),
            )
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(store.get(&s, &id).unwrap().data, Bytes::from_static(b"hi"));
        for other in [
            Scope {
                subject: "other".into(),
                ..s.clone()
            },
            Scope {
                session: "other".into(),
                ..s.clone()
            },
            Scope {
                agent: "other".into(),
                ..s.clone()
            },
        ] {
            assert!(matches!(store.get(&other, &id), Err(StatusCode::NOT_FOUND)));
        }
        store.items.lock().get_mut(&id).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(matches!(store.get(&s, &id), Err(StatusCode::NOT_FOUND)));
    }
    #[test]
    fn bounds_and_text_media_markers_are_enforced() {
        let store = Store::default();
        let s = scope();
        assert!(
            store
                .insert(
                    s.clone(),
                    "x".into(),
                    "text/plain".into(),
                    Bytes::from(vec![b'a'; MAX_TEXT_BYTES + 1])
                )
                .is_err()
        );
        let id = store
            .insert(
                s.clone(),
                "note.txt".into(),
                "text/plain".into(),
                Bytes::from_static(
                    b"hi [1,2] [IMAGE:/private/secret.png] [AUDIO:https://example.test/file] [IMAGE:/private/unclosed",
                ),
            )
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let prompt = store.materialize(&s, &[id], "read").unwrap();
        assert!(!prompt.contains("[IMAGE:"));
        assert!(!prompt.contains("[AUDIO:"));
        assert!(prompt.contains("hi [1,2]"));
        for _ in 1..MAX_SCOPE_ITEMS {
            store
                .insert(
                    s.clone(),
                    "x".into(),
                    "text/plain".into(),
                    Bytes::from_static(b"hi"),
                )
                .unwrap();
        }
        assert!(matches!(
            store.insert(
                s,
                "x".into(),
                "text/plain".into(),
                Bytes::from_static(b"hi")
            ),
            Err(StatusCode::TOO_MANY_REQUESTS)
        ));
    }
    #[tokio::test]
    async fn http_body_and_upload_admission_limits_fail_closed() {
        let mut config = zeroclaw_config::schema::Config::default();
        config.agents.insert(
            "assistant".into(),
            zeroclaw_config::schema::AliasedAgentConfig::default(),
        );
        let mut state = crate::api::test_state(config);
        state.pairing =
            std::sync::Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
                false,
                &["synthetic-paired".into()],
            ));
        let app = routes(state.clone(), 30);
        let request = |data: Vec<u8>| {
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/attachments?session_id=main&agent=assistant&file_name=photo.png")
                .header("authorization", "Bearer synthetic-paired")
                .header("content-type", "image/png")
                .body(Body::from(data))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request(vec![0; MAX_FILE_BYTES + 1]))
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let permits = state
            .ws_conversations
            .attachments
            .uploads
            .acquire_many(4)
            .await
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(request(b"\x89PNG\r\n\x1a\nfixture".to_vec()))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(permits);
        let mut bytes = vec![0; 65537];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            app.clone().oneshot(request(bytes)).await.unwrap().status(),
            StatusCode::CREATED,
            "attachment route supports more than the ordinary 64KiB JSON cap"
        );
        let pairing = state.pairing.clone();
        let body = Body::from_stream(futures_util::stream::once(async move {
            pairing.revoke_all_tokens();
            Ok::<_, std::io::Error>(Bytes::from_static(b"\x89PNG\r\n\x1a\nfixture"))
        }));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/attachments?session_id=main&agent=assistant&file_name=photo.png")
            .header("authorization", "Bearer synthetic-paired")
            .header("content-type", "image/png")
            .body(body)
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED,
            "revocation during body streaming must prevent insertion"
        );
    }

    #[tokio::test]
    async fn http_roundtrip_refuses_anonymous_wrong_scope_and_revoked_tokens() {
        let mut config = zeroclaw_config::schema::Config::default();
        config.agents.insert(
            "assistant".into(),
            zeroclaw_config::schema::AliasedAgentConfig::default(),
        );
        config.gateway.bridges.insert(
            "tg".into(),
            zeroclaw_config::schema::GatewayBridgeConfig {
                token_hash: zeroclaw_config::pairing::PairingGuard::token_hash("synthetic-token"),
                sessions: vec!["main".into(), "owner/用户 space".into()],
                ..Default::default()
            },
        );
        let state = crate::api::test_state(config);
        let app = routes(state.clone(), 30);
        let req = |method: &str, path: &str, token: Option<&str>, body: Body| {
            let mut r = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "text/plain");
            if let Some(t) = token {
                r = r.header("authorization", format!("Bearer {t}"));
            }
            r.body(body).unwrap()
        };
        let path = "/api/attachments?session_id=main&agent=assistant&file_name=note.txt";
        assert_eq!(
            app.clone()
                .oneshot(req("POST", path, None, Body::from("hello")))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(req(
                "POST",
                path,
                Some("synthetic-token"),
                Body::from("hello"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let info: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let id = info["id"].as_str().unwrap();
        let url = format!("/api/attachments/{id}?session_id=main&agent=assistant");
        let got = app
            .clone()
            .oneshot(req("GET", &url, Some("synthetic-token"), Body::empty()))
            .await
            .unwrap();
        assert_eq!(got.status(), StatusCode::OK);
        assert_eq!(to_bytes(got.into_body(), 4096).await.unwrap(), "hello");
        assert_eq!(
            app.clone()
                .oneshot(req(
                    "GET",
                    &url.replace("main", "other"),
                    Some("synthetic-token"),
                    Body::empty()
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone().oneshot(req("POST",
                "/api/attachments?session_id=owner%2F%E7%94%A8%E6%88%B7%20space&agent=assistant&file_name=note.txt",
                Some("synthetic-token"), Body::from("hello"))).await.unwrap().status(),
            StatusCode::CREATED,
            "attachment scopes preserve the existing session grammar"
        );
        state.config.write().gateway.bridges.clear();
        assert_eq!(
            app.oneshot(req("GET", &url, Some("synthetic-token"), Body::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
