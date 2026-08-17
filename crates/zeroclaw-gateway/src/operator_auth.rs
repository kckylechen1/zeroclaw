//! Shared operator-bearer gate for management API surfaces.
//!
//! Extracted from `api_node_identity` so non-nodes operator surfaces (the
//! User Model review API) enforce the identical contract: operator bearer
//! mandatory regardless of `require_pairing`, strict rate limiting on
//! presented-but-wrong tokens, and the pairing-rate ceiling after success.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json};
use std::net::SocketAddr;
use zeroclaw_runtime::security::pairing::{PairingGuard, constant_time_eq};

use crate::AppState;

pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
}

pub(crate) fn require_operator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    let token = extract_bearer(headers).unwrap_or("");
    if token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    let hashed = PairingGuard::token_hash(token);
    let known = state.pairing.tokens();
    if !known.iter().any(|stored| constant_time_eq(stored, &hashed)) {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    Ok(())
}

pub(crate) fn gate_operator_identity(
    state: &AppState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Option<axum::response::Response> {
    let rate_key =
        crate::client_key_from_request(Some(peer), headers, state.trust_forwarded_headers);
    let presented = extract_bearer(headers).is_some_and(|token| !token.is_empty());
    if let Err(err) = require_operator(state, headers) {
        if presented {
            if let Err(e) = state.auth_limiter.check_rate_limit_strict(&rate_key) {
                let body = serde_json::json!({
                    "error": format!("Too many auth attempts. Try again in {}s.", e.retry_after_secs),
                    "retry_after": e.retry_after_secs,
                });
                return Some((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
            }
        }
        state.auth_limiter.record_attempt_strict(&rate_key);
        return Some(err.into_response());
    }
    if let Err(e) = state.auth_limiter.check_rate_limit_strict(&rate_key) {
        let body = serde_json::json!({
            "error": format!("Too many auth attempts. Try again in {}s.", e.retry_after_secs),
            "retry_after": e.retry_after_secs,
        });
        return Some((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
    }
    if !state.rate_limiter.allow_pair(&rate_key) {
        let err = serde_json::json!({
            "error": "Too many pairing requests. Please retry later.",
            "retry_after": crate::RATE_LIMIT_WINDOW_SECS,
        });
        return Some((StatusCode::TOO_MANY_REQUESTS, Json(err)).into_response());
    }
    None
}
