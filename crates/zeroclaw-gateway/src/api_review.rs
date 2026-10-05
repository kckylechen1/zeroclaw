//! Unified owner review view. Soul and User Model stores remain canonical;
//! review actions use their existing operator-gated endpoints.

use crate::AppState;
use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use std::net::SocketAddr;
use zeroclaw_memory::companion::{SoulProfileStore, UserModelStore};

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/review/inbox", get(inbox))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxQuery {
    agent: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}
fn default_limit() -> usize {
    100
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"code": code, "error": message})),
    )
        .into_response()
}

pub async fn inbox(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let query = match axum::extract::Query::<InboxQuery>::try_from_uri(&uri) {
        Ok(query) => query.0,
        Err(err) => return error(StatusCode::BAD_REQUEST, "bad_query", &err.to_string()),
    };
    if query.limit == 0 || query.limit > 200 {
        return error(StatusCode::BAD_REQUEST, "bad_limit", "limit must be 1..200");
    }
    let (data_dir, mut agents) = {
        let config = state.config.read();
        let agents = match query.agent.as_deref() {
            Some(agent) if config.agent(agent).is_some() => vec![agent.to_string()],
            Some(_) => {
                return error(
                    StatusCode::NOT_FOUND,
                    "unknown_agent",
                    "unknown configured agent",
                );
            }
            None => config.agents.keys().cloned().collect::<Vec<_>>(),
        };
        (config.data_dir.clone(), agents)
    };
    agents.sort();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let soul = SoulProfileStore::shared(&data_dir)?;
        let user = UserModelStore::shared(&data_dir)?;
        let mut pending = Vec::new();
        let mut receipts = Vec::new();
        for agent in agents {
            for proposal in soul.proposals(&agent, true)? {
                pending.push(serde_json::json!({
                    "id": format!("soul:{agent}:{}", proposal.id), "kind": "soul_proposal", "agent": agent,
                    "created_at_unix": proposal.created_at_unix, "item": proposal,
                    "review_url": format!("/api/soul/proposals/{}/resolve", proposal.id),
                }));
            }
            for (id, receipt) in soul.reflections(&agent, 20)? {
                receipts.push(serde_json::json!({"id": format!("reflection:{agent}:{id}"), "kind": "reflection_receipt", "agent": agent, "created_at_unix": receipt.ran_at_unix, "item": receipt}));
            }
        }
        // The owner's User Model is shared; agent filtering only filters Soul
        // and reflection history, never hides global owner candidates.
        for candidate in user.list_pending_candidates()? {
            pending.push(serde_json::json!({"id": format!("user_model:{}", candidate.id), "kind": "user_model_candidate", "created_at_unix": candidate.created_at_unix, "item": candidate, "review_url": format!("/api/user-model/candidates/{}/review", candidate.id)}));
        }
        pending.sort_by(|a, b| (a["created_at_unix"].as_u64(), a["id"].as_str()).cmp(&(b["created_at_unix"].as_u64(), b["id"].as_str())));
        receipts.sort_by(|a, b| (b["created_at_unix"].as_u64(), b["id"].as_str()).cmp(&(a["created_at_unix"].as_u64(), a["id"].as_str())));
        pending.extend(receipts);
        let total = pending.len();
        let items: Vec<_> = pending.into_iter().skip(query.offset).take(query.limit).collect();
        let next = query.offset.saturating_add(items.len());
        Ok(serde_json::json!({"items": items, "total": total, "next_offset": if next < total { Some(next) } else { None }}))
    }).await;
    match result {
        Ok(Ok(body)) => (StatusCode::OK, axum::Json(body)).into_response(),
        Ok(Err(err)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_unavailable",
            &err.to_string(),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "task_failed",
            "store task failed",
        ),
    }
}

#[cfg(test)]
mod tests;
