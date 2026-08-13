//! In-process MCP MRTR task handles.
//!
//! A well-formed Modern [`McpResultKind::InputRequired`] is minted into a
//! handle the tool loop can continue. The model supplies answers on a later
//! round; the client retries the original JSON-RPC call with `inputResponses`
//! and the echoed opaque `requestState` (see [`crate::mcp_era::attach_input_retry`]).
//!
//! Lifecycle (process-local; a restart discards the table — the server holds
//! `requestState`, so that is acceptable):
//! - bounded count and TTL
//! - bound to the original method + params fingerprint
//! - single-consume on redeem
//!
//! [`PeerEra`](crate::mcp_era::PeerEra) stays the only version dispatch:
//! Legacy peers never mint handles. This store does not persist and does not
//! implement the `io.modelcontextprotocol/tasks` extension.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::mcp_era::{InputRequired, RESULT_TYPE_INPUT_REQUIRED};

/// Reserved tool-argument key the model uses to continue a pending handle.
pub const TASK_HANDLE_ARG: &str = "mcpTaskHandle";
/// Wire / argument field carrying client answers keyed like `inputRequests`.
pub const INPUT_RESPONSES_FIELD: &str = "inputResponses";

/// Default in-process table size. Excess mint attempts fail closed.
pub const MAX_TASK_HANDLES: usize = 32;
/// Default handle lifetime. Expired redeem is fail-closed.
pub const TASK_HANDLE_TTL: Duration = Duration::from_secs(300);
/// Opaque `requestState` is stored for exact echo. Larger blobs fail closed
/// at mint so a malicious server cannot pin unbounded memory.
pub const MAX_REQUEST_STATE_BYTES: usize = 64 * 1024;

/// Model-visible continuation after a well-formed `input_required`.
///
/// This is not a completed tool result. `requestState` is never included in
/// [`Display`]: it is attacker-controlled and must only be echoed on retry.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTaskPending {
    pub handle: String,
    pub method: String,
    pub input_required: InputRequired,
    pub ttl_secs: u64,
}

impl std::fmt::Display for McpTaskPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP `{}` returned resultType={RESULT_TYPE_INPUT_REQUIRED}; \
             continue with {TASK_HANDLE_ARG}={} (single-use, TTL {}s)",
            self.method, self.handle, self.ttl_secs
        )?;
        if let Some(map) = self.input_required.input_requests.as_ref()
            && !map.is_empty()
        {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            write!(f, "; inputRequests keys: {}", keys.join(","))?;
            let raw = serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string());
            write!(
                f,
                "; inputRequests={}",
                zeroclaw_providers::sanitize_api_error(&raw)
            )?;
            write!(
                f,
                "; retry this tool with {TASK_HANDLE_ARG} and {INPUT_RESPONSES_FIELD} \
                 (same keys; do not send requestState)"
            )?;
        } else {
            write!(
                f,
                "; retry this tool with {TASK_HANDLE_ARG} (no inputResponses required)"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for McpTaskPending {}

/// Why a handle was not minted or not redeemed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskHandleError {
    Unknown,
    Expired,
    BindingMismatch,
    Capacity,
    RequestStateTooLarge,
    MalformedHandle,
    MalformedInputResponses,
    MissingInputResponses,
}

impl std::fmt::Display for TaskHandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown MCP task handle"),
            Self::Expired => write!(f, "MCP task handle expired"),
            Self::BindingMismatch => {
                write!(f, "MCP task handle does not match the original request")
            }
            Self::Capacity => write!(
                f,
                "MCP task handle table is full (max {MAX_TASK_HANDLES}); retry later"
            ),
            Self::RequestStateTooLarge => write!(
                f,
                "MCP requestState exceeds {MAX_REQUEST_STATE_BYTES} bytes; refused"
            ),
            Self::MalformedHandle => {
                write!(f, "MCP `{TASK_HANDLE_ARG}` must be a non-empty string")
            }
            Self::MalformedInputResponses => {
                write!(f, "MCP `{INPUT_RESPONSES_FIELD}` must be a JSON object")
            }
            Self::MissingInputResponses => write!(
                f,
                "MCP task handle requires `{INPUT_RESPONSES_FIELD}` for the pending inputRequests"
            ),
        }
    }
}

impl std::error::Error for TaskHandleError {}

/// Answers the model supplied to continue a pending handle.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskContinuation {
    pub handle: String,
    pub input_responses: Option<serde_json::Value>,
}

/// Original request a handle will retry, plus the opaque server state.
#[derive(Debug, Clone, PartialEq)]
pub struct RedeemedTask {
    pub method: String,
    pub params: serde_json::Value,
    pub input_required: InputRequired,
}

struct TaskRecord {
    method: String,
    params: serde_json::Value,
    fingerprint: [u8; 32],
    input_required: InputRequired,
    expires_at: Instant,
}

/// Process-local handle table. Not durable across restart.
pub struct McpTaskStore {
    handles: HashMap<String, TaskRecord>,
    max_handles: usize,
    ttl: Duration,
}

impl Default for McpTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl McpTaskStore {
    pub fn new() -> Self {
        Self::with_limits(MAX_TASK_HANDLES, TASK_HANDLE_TTL)
    }

    pub fn with_limits(max_handles: usize, ttl: Duration) -> Self {
        Self {
            handles: HashMap::new(),
            max_handles: max_handles.max(1),
            ttl,
        }
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// Mint a handle for a well-formed `input_required`. Fails closed on
    /// capacity or oversized `requestState`. Does not inspect state bytes.
    pub fn mint(
        &mut self,
        method: &str,
        params: serde_json::Value,
        input_required: InputRequired,
    ) -> Result<McpTaskPending, TaskHandleError> {
        if let Some(state) = input_required.request_state.as_ref()
            && state.len() > MAX_REQUEST_STATE_BYTES
        {
            return Err(TaskHandleError::RequestStateTooLarge);
        }
        self.sweep_expired();
        if self.handles.len() >= self.max_handles {
            return Err(TaskHandleError::Capacity);
        }
        let fingerprint = request_fingerprint(method, &params);
        let handle = format!("mcp-task-{}", uuid::Uuid::new_v4().simple());
        let ttl_secs = self.ttl.as_secs();
        self.handles.insert(
            handle.clone(),
            TaskRecord {
                method: method.to_string(),
                params,
                fingerprint,
                input_required: input_required.clone(),
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(McpTaskPending {
            handle,
            method: method.to_string(),
            input_required,
            ttl_secs,
        })
    }

    /// Single-consume redeem. Unknown, expired, or binding mismatch fail
    /// closed and do not leave a reusable handle behind.
    pub fn redeem(
        &mut self,
        handle: &str,
        expected_method: &str,
        expected_binding: Option<&str>,
    ) -> Result<RedeemedTask, TaskHandleError> {
        let Some(record) = self.handles.remove(handle) else {
            return Err(TaskHandleError::Unknown);
        };
        if Instant::now() >= record.expires_at {
            return Err(TaskHandleError::Expired);
        }
        if record.method != expected_method {
            return Err(TaskHandleError::BindingMismatch);
        }
        if request_fingerprint(&record.method, &record.params) != record.fingerprint {
            return Err(TaskHandleError::BindingMismatch);
        }
        if let Some(expected) = expected_binding {
            let stored = binding_value(&record.method, &record.params);
            if stored != Some(expected) {
                return Err(TaskHandleError::BindingMismatch);
            }
        }
        Ok(RedeemedTask {
            method: record.method,
            params: record.params,
            input_required: record.input_required,
        })
    }

    fn sweep_expired(&mut self) {
        let now = Instant::now();
        self.handles.retain(|_, record| record.expires_at > now);
    }
}

/// SHA-256 of `method` + canonical JSON params. Used only as an in-process
/// bind; not a wire field.
pub fn request_fingerprint(method: &str, params: &serde_json::Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update([0u8]);
    hasher.update(params.to_string().as_bytes());
    hasher.finalize().into()
}

fn binding_value<'a>(method: &str, params: &'a serde_json::Value) -> Option<&'a str> {
    match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(serde_json::Value::as_str),
        "resources/read" => params.get("uri").and_then(serde_json::Value::as_str),
        _ => None,
    }
}

/// Pull a continuation out of model-supplied tool arguments.
///
/// Absence of [`TASK_HANDLE_ARG`] means a fresh call. A present but malformed
/// handle fails closed rather than falling through as ordinary arguments.
pub fn parse_continuation(
    args: &serde_json::Value,
) -> Result<Option<TaskContinuation>, TaskHandleError> {
    let Some(obj) = args.as_object() else {
        return Ok(None);
    };
    match obj.get(TASK_HANDLE_ARG) {
        None => Ok(None),
        Some(serde_json::Value::String(id)) if !id.is_empty() => {
            let input_responses = match obj.get(INPUT_RESPONSES_FIELD) {
                None => None,
                Some(value) if value.is_object() => Some(value.clone()),
                Some(_) => return Err(TaskHandleError::MalformedInputResponses),
            };
            Ok(Some(TaskContinuation {
                handle: id.clone(),
                input_responses,
            }))
        }
        Some(_) => Err(TaskHandleError::MalformedHandle),
    }
}

/// Spec: if `inputRequests` was present, the client must construct answers
/// before retrying.
pub fn require_responses_if_needed(
    input_required: &InputRequired,
    input_responses: &Option<serde_json::Value>,
) -> Result<(), TaskHandleError> {
    let needs_answers = input_required
        .input_requests
        .as_ref()
        .is_some_and(|map| !map.is_empty());
    if needs_answers && input_responses.is_none() {
        return Err(TaskHandleError::MissingInputResponses);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_required(state: Option<&str>) -> InputRequired {
        InputRequired {
            input_requests: Some(serde_json::Map::from_iter([(
                "github_login".to_string(),
                json!({
                    "method": "elicitation/create",
                    "params": {"mode": "form", "message": "name"}
                }),
            )])),
            request_state: state.map(str::to_string),
        }
    }

    fn call_params(name: &str) -> serde_json::Value {
        json!({ "name": name, "arguments": {"q": 1} })
    }

    #[test]
    fn mint_then_redeem_consumes_once() {
        let mut store = McpTaskStore::new();
        let pending = store
            .mint(
                "tools/call",
                call_params("echo"),
                sample_required(Some("blob")),
            )
            .expect("mint");
        assert_eq!(store.len(), 1);
        let redeemed = store
            .redeem(&pending.handle, "tools/call", Some("echo"))
            .expect("redeem");
        assert_eq!(redeemed.method, "tools/call");
        assert_eq!(redeemed.params, call_params("echo"));
        assert_eq!(
            redeemed.input_required.request_state.as_deref(),
            Some("blob")
        );
        assert!(store.is_empty());
        assert_eq!(
            store.redeem(&pending.handle, "tools/call", Some("echo")),
            Err(TaskHandleError::Unknown)
        );
    }

    #[test]
    fn unknown_handle_fails_closed() {
        let mut store = McpTaskStore::new();
        assert_eq!(
            store.redeem("mcp-task-deadbeef", "tools/call", Some("echo")),
            Err(TaskHandleError::Unknown)
        );
    }

    #[test]
    fn expired_handle_fails_closed() {
        let mut store = McpTaskStore::with_limits(4, Duration::from_millis(1));
        let pending = store
            .mint("tools/call", call_params("echo"), sample_required(None))
            .expect("mint");
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            store.redeem(&pending.handle, "tools/call", Some("echo")),
            Err(TaskHandleError::Expired)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn wrong_tool_name_fails_closed_and_consumes() {
        let mut store = McpTaskStore::new();
        let pending = store
            .mint("tools/call", call_params("echo"), sample_required(None))
            .expect("mint");
        assert_eq!(
            store.redeem(&pending.handle, "tools/call", Some("other")),
            Err(TaskHandleError::BindingMismatch)
        );
        assert_eq!(
            store.redeem(&pending.handle, "tools/call", Some("echo")),
            Err(TaskHandleError::Unknown)
        );
    }

    #[test]
    fn wrong_method_fails_closed() {
        let mut store = McpTaskStore::new();
        let pending = store
            .mint("tools/call", call_params("echo"), sample_required(None))
            .expect("mint");
        assert_eq!(
            store.redeem(&pending.handle, "prompts/get", Some("echo")),
            Err(TaskHandleError::BindingMismatch)
        );
    }

    #[test]
    fn capacity_fails_closed() {
        let mut store = McpTaskStore::with_limits(1, TASK_HANDLE_TTL);
        store
            .mint("tools/call", call_params("a"), sample_required(None))
            .expect("first");
        assert_eq!(
            store.mint("tools/call", call_params("b"), sample_required(None)),
            Err(TaskHandleError::Capacity)
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn oversized_request_state_fails_closed() {
        let mut store = McpTaskStore::new();
        let huge = "x".repeat(MAX_REQUEST_STATE_BYTES + 1);
        let err = store
            .mint(
                "tools/call",
                call_params("echo"),
                sample_required(Some(&huge)),
            )
            .expect_err("too large");
        assert_eq!(err, TaskHandleError::RequestStateTooLarge);
        assert!(store.is_empty());
        let msg = err.to_string();
        assert!(!msg.contains(&huge), "blob leaked into error");
        assert!(msg.len() < 200, "error should be short, got {}", msg.len());
    }

    #[test]
    fn pending_display_omits_request_state_and_bounds_requests() {
        let huge_state = "S".repeat(8000);
        let huge_message = "M".repeat(8000);
        let pending = McpTaskPending {
            handle: "mcp-task-abc".into(),
            method: "tools/call".into(),
            input_required: InputRequired {
                input_requests: Some(serde_json::Map::from_iter([(
                    "q".to_string(),
                    json!({
                        "method": "elicitation/create",
                        "params": {"mode": "form", "message": huge_message}
                    }),
                )])),
                request_state: Some(huge_state.clone()),
            },
            ttl_secs: 300,
        };
        let msg = pending.to_string();
        assert!(msg.contains("mcp-task-abc"), "got: {msg}");
        assert!(msg.contains(TASK_HANDLE_ARG), "got: {msg}");
        assert!(msg.contains("inputRequests"), "got: {msg}");
        assert!(
            !msg.contains(&huge_state),
            "requestState must not be model-visible"
        );
        assert!(
            !msg.contains(&"M".repeat(600)),
            "huge inputRequests not bounded: len={}",
            msg.len()
        );
        assert!(msg.contains("..."), "bounded detail should truncate: {msg}");
    }

    #[test]
    fn parse_continuation_fresh_call_is_none() {
        assert_eq!(parse_continuation(&json!({"q": 1})).expect("ok"), None);
        assert_eq!(parse_continuation(&json!([])).expect("ok"), None);
    }

    #[test]
    fn parse_continuation_reads_handle_and_responses() {
        let cont = parse_continuation(&json!({
            TASK_HANDLE_ARG: "mcp-task-1",
            INPUT_RESPONSES_FIELD: {"github_login": {"action": "accept"}}
        }))
        .expect("ok")
        .expect("present");
        assert_eq!(cont.handle, "mcp-task-1");
        assert!(cont.input_responses.is_some());
    }

    #[test]
    fn parse_continuation_malformed_handle_fails_closed() {
        assert_eq!(
            parse_continuation(&json!({ TASK_HANDLE_ARG: 1 })),
            Err(TaskHandleError::MalformedHandle)
        );
        assert_eq!(
            parse_continuation(&json!({ TASK_HANDLE_ARG: "" })),
            Err(TaskHandleError::MalformedHandle)
        );
    }

    #[test]
    fn parse_continuation_malformed_responses_fails_closed() {
        assert_eq!(
            parse_continuation(&json!({
                TASK_HANDLE_ARG: "mcp-task-1",
                INPUT_RESPONSES_FIELD: "not-an-object"
            })),
            Err(TaskHandleError::MalformedInputResponses)
        );
    }

    #[test]
    fn missing_responses_required_when_input_requests_present() {
        let ir = sample_required(Some("blob"));
        assert_eq!(
            require_responses_if_needed(&ir, &None),
            Err(TaskHandleError::MissingInputResponses)
        );
        assert!(require_responses_if_needed(&ir, &Some(json!({}))).is_ok());
    }

    #[test]
    fn request_state_only_allows_empty_responses() {
        let ir = InputRequired {
            input_requests: None,
            request_state: Some("load-shed".into()),
        };
        assert!(require_responses_if_needed(&ir, &None).is_ok());
    }

    #[test]
    fn fingerprint_stable_for_same_params() {
        let params = call_params("echo");
        assert_eq!(
            request_fingerprint("tools/call", &params),
            request_fingerprint("tools/call", &params)
        );
        assert_ne!(
            request_fingerprint("tools/call", &params),
            request_fingerprint("prompts/get", &params)
        );
    }
}
