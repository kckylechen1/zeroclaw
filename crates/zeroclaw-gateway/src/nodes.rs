//! WebSocket endpoint for dynamic node discovery and capability advertisement.

use super::AppState;
use axum::{
    Json,
    extract::{
        ConnectInfo, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zeroclaw_api::node::{
    GatewayToNode, NodeErrorCode, NodeToGateway, WS_NODES_V2, is_v1_register_frame,
    negotiate_v2_minor,
};
use zeroclaw_runtime::security::pairing::{PairingGuard, constant_time_eq};

pub mod mdns;

/// Prefix used in `Sec-WebSocket-Protocol` to carry a bearer token.
const BEARER_SUBPROTO_PREFIX: &str = "bearer.";

const NODES_DISABLED_MSG: &str =
    "Not Found — node discovery is disabled (set nodes.enabled=true to enable)";

/// A single capability advertised by a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapability {
    pub name: String,
    pub description: String,
    #[serde(default = "default_capability_parameters")]
    pub parameters: serde_json::Value,
}

fn default_capability_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {}
    })
}

/// Tracks a connected node and its capabilities.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub node_id: String,
    pub capabilities: Vec<NodeCapability>,
    /// Channel to send invocation requests to the node's WebSocket handler.
    pub invoke_tx: mpsc::Sender<NodeInvocation>,
}

/// An invocation request sent to a node.
#[derive(Debug)]
pub struct NodeInvocation {
    pub call_id: String,
    pub capability: String,
    pub args: serde_json::Value,
    pub response_tx: oneshot::Sender<NodeInvocationResult>,
}

/// The result of a node invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInvocationResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

/// Per-socket identity minted at v2 HelloAck. Later slices tear down by this pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConnection {
    pub connection_id: String,
    pub generation: u64,
}

/// Registry of all connected nodes and their capabilities.
#[derive(Debug, Default, Clone)]
pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
    max_nodes: usize,
    next_generation: Arc<AtomicU64>,
}

impl NodeRegistry {
    /// Create a new registry with the given capacity limit.
    pub fn new(max_nodes: usize) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            max_nodes,
            next_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Allocate a connection id and monotonic generation for one socket.
    pub fn mint_connection(&self) -> NodeConnection {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        NodeConnection {
            connection_id: Uuid::new_v4().to_string(),
            generation,
        }
    }

    /// Register a node with its capabilities. Returns false if at capacity.
    pub fn register(&self, info: NodeInfo) -> bool {
        let mut nodes = self.nodes.write();
        if nodes.len() >= self.max_nodes && !nodes.contains_key(&info.node_id) {
            return false;
        }
        nodes.insert(info.node_id.clone(), info);
        true
    }

    /// Remove a node from the registry.
    pub fn unregister(&self, node_id: &str) {
        self.nodes.write().remove(node_id);
    }

    /// List all registered node IDs.
    pub fn node_ids(&self) -> Vec<String> {
        self.nodes.read().keys().cloned().collect()
    }

    /// Get all capabilities across all nodes, keyed by prefixed tool name.
    pub fn all_capabilities(&self) -> Vec<(String, String, NodeCapability)> {
        let nodes = self.nodes.read();
        let mut caps = Vec::new();
        for info in nodes.values() {
            for cap in &info.capabilities {
                caps.push((info.node_id.clone(), cap.name.clone(), cap.clone()));
            }
        }
        caps
    }

    /// Get the invocation sender for a specific node.
    pub fn invoke_tx(&self, node_id: &str) -> Option<mpsc::Sender<NodeInvocation>> {
        self.nodes.read().get(node_id).map(|n| n.invoke_tx.clone())
    }

    /// Check if a node is registered.
    pub fn contains(&self, node_id: &str) -> bool {
        self.nodes.read().contains_key(node_id)
    }

    /// Number of registered nodes.
    pub fn len(&self) -> usize {
        self.nodes.read().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.read().is_empty()
    }
}

/// Query parameters for the `/ws/nodes` endpoint.
#[derive(Deserialize)]
pub struct NodeWsQuery {
    pub token: Option<String>,
}

/// Extract a bearer token from WebSocket-compatible sources.
fn extract_node_ws_token<'a>(
    headers: &'a HeaderMap,
    query_token: Option<&'a str>,
) -> Option<&'a str> {
    // 1. Authorization header
    if let Some(t) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        && !t.is_empty()
    {
        return Some(t);
    }

    // 2. Sec-WebSocket-Protocol: bearer.<token>
    if let Some(t) = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .and_then(|protos| {
            protos
                .split(',')
                .map(|p| p.trim())
                .find_map(|p| p.strip_prefix(BEARER_SUBPROTO_PREFIX))
        })
        && !t.is_empty()
    {
        return Some(t);
    }

    // 3. ?token= query parameter
    if let Some(t) = query_token
        && !t.is_empty()
    {
        return Some(t);
    }

    None
}

pub(crate) fn check_node_auth(
    nodes_config: &zeroclaw_config::schema::NodesConfig,
    pairing: &PairingGuard,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Option<(axum::http::StatusCode, &'static str)> {
    if !nodes_config.enabled {
        return Some((axum::http::StatusCode::NOT_FOUND, NODES_DISABLED_MSG));
    }
    if let Some(ref expected_token) = nodes_config.auth_token {
        // Fail-closed: a whitespace-only / empty configured token must not
        // authenticate missing or arbitrary tokens (trimming both sides
        // would produce `constant_time_eq("", "")` = true and bypass auth).
        if expected_token.trim().is_empty() {
            return Some((
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Service Unavailable — nodes.auth_token must not be empty or whitespace-only",
            ));
        }
        let token = extract_node_ws_token(headers, query_token).unwrap_or("");
        // SECURITY: route through `constant_time_eq` (not `==`) to prevent
        // a remote timing attack that could leak `nodes.auth_token` one
        // byte at a time. Both sides are `.trim()`-normalized to match
        // the canonical pattern at
        // `crates/zeroclaw-config/src/pairing.rs:139`.
        if !constant_time_eq(token.trim(), expected_token.trim()) {
            return Some((
                axum::http::StatusCode::UNAUTHORIZED,
                "Unauthorized — provide a valid node auth token",
            ));
        }
    } else if pairing.require_pairing() {
        let token = extract_node_ws_token(headers, query_token).unwrap_or("");
        if !pairing.is_authenticated(token) {
            return Some((
                axum::http::StatusCode::UNAUTHORIZED,
                "Unauthorized — provide Authorization header or ?token= query param",
            ));
        }
    } else {
        return Some((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable — node registration is disabled because no auth method is configured. \
             Set nodes.auth_token OR enable gateway.require_pairing.",
        ));
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeWsAdmission {
    Ok,
    Legacy {
        status: StatusCode,
        body: &'static str,
    },
    Typed {
        status: StatusCode,
        code: NodeErrorCode,
    },
}

#[derive(Serialize)]
struct NodeProtocolReject {
    code: NodeErrorCode,
}

fn client_offers_subprotocol(headers: &HeaderMap, protocol: &str) -> bool {
    headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|protos| protos.split(',').any(|p| p.trim() == protocol))
}

fn peer_is_loopback(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// HTTP admission for `/ws/nodes`: enabled → loopback → auth → v2 subprotocol.
fn admit_node_ws(
    nodes_config: &zeroclaw_config::schema::NodesConfig,
    pairing: &PairingGuard,
    headers: &HeaderMap,
    query_token: Option<&str>,
    peer: SocketAddr,
) -> NodeWsAdmission {
    if !nodes_config.enabled {
        return NodeWsAdmission::Legacy {
            status: StatusCode::NOT_FOUND,
            body: NODES_DISABLED_MSG,
        };
    }
    if !peer_is_loopback(peer) {
        return NodeWsAdmission::Typed {
            status: StatusCode::FORBIDDEN,
            code: NodeErrorCode::LoopbackRequired,
        };
    }
    if let Some((status, body)) = check_node_auth(nodes_config, pairing, headers, query_token) {
        return NodeWsAdmission::Legacy { status, body };
    }
    if !client_offers_subprotocol(headers, WS_NODES_V2) {
        return NodeWsAdmission::Typed {
            status: StatusCode::BAD_REQUEST,
            code: NodeErrorCode::ProtocolUnsupported,
        };
    }
    NodeWsAdmission::Ok
}

#[derive(Debug, Clone, PartialEq)]
enum NodeHandshakeOutcome {
    Ack(GatewayToNode),
    Reject {
        frame: GatewayToNode,
        close_reason: &'static str,
    },
}

fn protocol_error_frame(code: NodeErrorCode) -> GatewayToNode {
    GatewayToNode::Error {
        code,
        retryable: false,
        call_id: None,
        detail: None,
    }
}

fn handshake_reject(code: NodeErrorCode) -> NodeHandshakeOutcome {
    NodeHandshakeOutcome::Reject {
        frame: protocol_error_frame(code),
        close_reason: code.as_str(),
    }
}

/// First in-band frame after a v2 upgrade: Hello with overlapping minors, or reject.
fn handshake_first_frame(text: &str, conn: &NodeConnection) -> NodeHandshakeOutcome {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return handshake_reject(NodeErrorCode::ProtocolUnsupported);
    };
    if is_v1_register_frame(&value) {
        return handshake_reject(NodeErrorCode::ProtocolUnsupported);
    }
    let Ok(frame) = serde_json::from_value::<NodeToGateway>(value) else {
        return handshake_reject(NodeErrorCode::ProtocolUnsupported);
    };
    match frame {
        NodeToGateway::Hello {
            protocol_versions, ..
        } => match negotiate_v2_minor(&protocol_versions) {
            Some(version) => NodeHandshakeOutcome::Ack(GatewayToNode::HelloAck {
                protocol_version: version.to_string(),
                connection_id: conn.connection_id.clone(),
                generation: conn.generation,
            }),
            None => handshake_reject(NodeErrorCode::VersionMismatch),
        },
        _ => handshake_reject(NodeErrorCode::ProtocolUnsupported),
    }
}

fn inbound_is_v1_register(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| is_v1_register_frame(&value))
}

pub async fn handle_ws_nodes(
    State(state): State<AppState>,
    Query(params): Query<NodeWsQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let nodes_config = state.config.read().nodes.clone();
    match admit_node_ws(
        &nodes_config,
        &state.pairing,
        &headers,
        params.token.as_deref(),
        peer,
    ) {
        NodeWsAdmission::Ok => {}
        NodeWsAdmission::Legacy { status, body } => return (status, body).into_response(),
        NodeWsAdmission::Typed { status, code } => {
            return (status, Json(NodeProtocolReject { code })).into_response();
        }
    }

    let registry = state.node_registry.clone();
    ws.protocols([WS_NODES_V2])
        .on_upgrade(move |socket| handle_node_socket(socket, registry))
        .into_response()
}

async fn handle_node_socket(socket: WebSocket, registry: Arc<NodeRegistry>) {
    let (mut sender, mut receiver) = socket.split();
    let conn = registry.mint_connection();

    let first_text = loop {
        match receiver.next().await {
            Some(Ok(Message::Text(text))) => break text.to_string(),
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
            Some(Ok(_)) => continue,
        }
    };

    match handshake_first_frame(&first_text, &conn) {
        NodeHandshakeOutcome::Ack(ack) => {
            if send_json(&mut sender, &ack).await.is_err() {
                return;
            }
        }
        NodeHandshakeOutcome::Reject {
            frame,
            close_reason,
        } => {
            let _ = send_json(&mut sender, &frame).await;
            let _ = close_protocol(&mut sender, close_reason).await;
            return;
        }
    }

    loop {
        let text = match receiver.next().await {
            Some(Ok(Message::Text(text))) => text.to_string(),
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            Some(Ok(_)) => continue,
        };
        if inbound_is_v1_register(&text) {
            let _ = send_json(
                &mut sender,
                &protocol_error_frame(NodeErrorCode::ProtocolUnsupported),
            )
            .await;
            let _ = close_protocol(&mut sender, NodeErrorCode::ProtocolUnsupported.as_str()).await;
            return;
        }
    }
}

async fn send_json<T: Serialize>(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    value: &T,
) -> Result<(), ()> {
    let json = serde_json::to_string(value).map_err(|_| ())?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

async fn close_protocol(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    reason: &'static str,
) -> Result<(), ()> {
    sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1002,
            reason: axum::extract::ws::Utf8Bytes::from_static(reason),
        })))
        .await
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use zeroclaw_api::node::{GrantProof, WS_NODES_V1};
    use zeroclaw_config::schema::NodesConfig;
    use zeroclaw_runtime::security::pairing::PairingGuard;

    // ── Auth matrix tests (via check_node_auth — no WS handshake required) ──

    fn empty_headers() -> HeaderMap {
        HeaderMap::new()
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        h
    }

    fn make_pairing(require: bool) -> PairingGuard {
        PairingGuard::new(require, &[])
    }

    #[test]
    fn nodes_disabled_returns_404() {
        let cfg = NodesConfig {
            enabled: false,
            ..NodesConfig::default()
        };
        let result = check_node_auth(&cfg, &make_pairing(false), &empty_headers(), None);
        assert_eq!(result.map(|(s, _)| s), Some(StatusCode::NOT_FOUND));
    }

    #[test]
    fn nodes_enabled_no_auth_no_pairing_returns_503() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: None,
            ..NodesConfig::default()
        };
        let result = check_node_auth(&cfg, &make_pairing(false), &empty_headers(), None);
        assert_eq!(
            result.map(|(s, _)| s),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    /// nodes.auth_token set to whitespace-only, no token provided → 503
    /// (fail-closed: empty-trimmed config must not match missing token).
    #[test]
    fn nodes_auth_token_whitespace_only_rejects_missing_token() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: Some("   ".into()),
            ..NodesConfig::default()
        };
        let result = check_node_auth(&cfg, &make_pairing(false), &empty_headers(), None);
        assert_eq!(
            result.map(|(s, _)| s),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    /// nodes.auth_token set to whitespace-only, caller presents a token → 503
    /// (fail-closed: empty-trimmed config must not authenticate any token).
    #[test]
    fn nodes_auth_token_whitespace_only_rejects_any_token() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: Some("   ".into()),
            ..NodesConfig::default()
        };
        let headers = bearer_headers("anything");
        let result = check_node_auth(&cfg, &make_pairing(false), &headers, None);
        assert_eq!(
            result.map(|(s, _)| s),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    /// nodes.auth_token set to whitespace-only, caller presents matching
    /// whitespace token → 503 (same fail-closed path).
    #[test]
    fn nodes_auth_token_whitespace_only_rejects_matching_whitespace_token() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: Some("   ".into()),
            ..NodesConfig::default()
        };
        let headers = bearer_headers("   ");
        let result = check_node_auth(&cfg, &make_pairing(false), &headers, None);
        assert_eq!(
            result.map(|(s, _)| s),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    /// nodes.auth_token set, caller presents wrong/missing token → 401.
    #[test]
    fn nodes_auth_token_wrong_token_returns_401() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: Some("secret".into()),
            ..NodesConfig::default()
        };
        let result = check_node_auth(&cfg, &make_pairing(false), &empty_headers(), None);
        assert_eq!(result.map(|(s, _)| s), Some(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn nodes_auth_token_correct_token_passes() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: Some("secret".into()),
            ..NodesConfig::default()
        };
        let headers = bearer_headers("secret");
        let result = check_node_auth(&cfg, &make_pairing(false), &headers, None);
        assert!(result.is_none(), "correct token must pass auth gate");
    }

    #[test]
    fn nodes_pairing_required_wrong_token_returns_401() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: None,
            ..NodesConfig::default()
        };
        let result = check_node_auth(&cfg, &make_pairing(true), &empty_headers(), None);
        assert_eq!(result.map(|(s, _)| s), Some(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn node_registry_register_and_unregister() {
        let registry = NodeRegistry::new(10);
        let (tx, _rx) = mpsc::channel(1);

        let info = NodeInfo {
            node_id: "test-node".to_string(),
            capabilities: vec![NodeCapability {
                name: "ping".to_string(),
                description: "Ping test".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            invoke_tx: tx,
        };

        assert!(registry.register(info));
        assert!(registry.contains("test-node"));
        assert_eq!(registry.len(), 1);

        registry.unregister("test-node");
        assert!(!registry.contains("test-node"));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn node_registry_capacity_limit() {
        let registry = NodeRegistry::new(2);

        for i in 0..2 {
            let (tx, _rx) = mpsc::channel(1);
            let info = NodeInfo {
                node_id: format!("node-{i}"),
                capabilities: vec![],
                invoke_tx: tx,
            };
            assert!(registry.register(info));
        }

        let (tx, _rx) = mpsc::channel(1);
        let info = NodeInfo {
            node_id: "node-overflow".to_string(),
            capabilities: vec![],
            invoke_tx: tx,
        };
        assert!(!registry.register(info));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn node_registry_re_register_same_id() {
        let registry = NodeRegistry::new(2);
        let (tx1, _rx1) = mpsc::channel(1);
        let (tx2, _rx2) = mpsc::channel(1);

        let info1 = NodeInfo {
            node_id: "node-1".to_string(),
            capabilities: vec![NodeCapability {
                name: "old".to_string(),
                description: "Old cap".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            invoke_tx: tx1,
        };
        assert!(registry.register(info1));

        let info2 = NodeInfo {
            node_id: "node-1".to_string(),
            capabilities: vec![NodeCapability {
                name: "new".to_string(),
                description: "New cap".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            invoke_tx: tx2,
        };
        // Re-registering same node_id should succeed (update)
        assert!(registry.register(info2));
        assert_eq!(registry.len(), 1);

        let caps = registry.all_capabilities();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].2.name, "new");
    }

    #[test]
    fn node_registry_all_capabilities() {
        let registry = NodeRegistry::new(10);
        let (tx1, _rx1) = mpsc::channel(1);
        let (tx2, _rx2) = mpsc::channel(1);

        registry.register(NodeInfo {
            node_id: "phone-1".to_string(),
            capabilities: vec![
                NodeCapability {
                    name: "camera.snap".to_string(),
                    description: "Take a photo".to_string(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                },
                NodeCapability {
                    name: "gps.location".to_string(),
                    description: "Get GPS location".to_string(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                },
            ],
            invoke_tx: tx1,
        });

        registry.register(NodeInfo {
            node_id: "sensor-1".to_string(),
            capabilities: vec![NodeCapability {
                name: "temp.read".to_string(),
                description: "Read temperature".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
            invoke_tx: tx2,
        });

        let caps = registry.all_capabilities();
        assert_eq!(caps.len(), 3);
    }

    #[test]
    fn node_registry_is_empty() {
        let registry = NodeRegistry::new(10);
        assert!(registry.is_empty());

        let (tx, _rx) = mpsc::channel(1);
        registry.register(NodeInfo {
            node_id: "n".to_string(),
            capabilities: vec![],
            invoke_tx: tx,
        });
        assert!(!registry.is_empty());
    }

    #[test]
    fn node_capability_deserialize() {
        let json = r#"{"name":"camera.snap","description":"Take a photo"}"#;
        let cap: NodeCapability = serde_json::from_str(json).unwrap();
        assert_eq!(cap.name, "camera.snap");
        assert_eq!(cap.description, "Take a photo");
        // Default parameters
        assert_eq!(cap.parameters["type"], "object");
    }

    #[test]
    fn nodes_v2_offer_handshake_succeeds() {
        let conn = test_conn();
        let outcome =
            handshake_first_frame(r#"{"type":"hello","protocol_versions":["2.0"]}"#, &conn);
        match outcome {
            NodeHandshakeOutcome::Ack(GatewayToNode::HelloAck {
                protocol_version,
                connection_id,
                generation,
            }) => {
                assert_eq!(protocol_version, "2.0");
                assert_eq!(connection_id, "conn-test");
                assert_eq!(generation, 7);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    #[test]
    fn nodes_v1_only_offer_is_typed_protocol_unsupported() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V1.parse().unwrap());
        match admit_node_ws(&cfg, &make_pairing(false), &headers, None, loopback_peer()) {
            NodeWsAdmission::Typed {
                status,
                code: NodeErrorCode::ProtocolUnsupported,
            } => assert_eq!(status, StatusCode::BAD_REQUEST),
            other => panic!("expected typed protocol_unsupported, got {other:?}"),
        }
    }

    #[test]
    fn nodes_missing_subprotocol_is_rejected_upgrade_hole_closed() {
        let cfg = enabled_secret_cfg();
        let headers = bearer_headers("secret");
        match admit_node_ws(&cfg, &make_pairing(false), &headers, None, loopback_peer()) {
            NodeWsAdmission::Typed {
                status,
                code: NodeErrorCode::ProtocolUnsupported,
            } => assert_eq!(status, StatusCode::BAD_REQUEST),
            other => panic!("missing subprotocol must not upgrade, got {other:?}"),
        }
    }

    #[test]
    fn nodes_bearer_subprotocol_without_v2_is_rejected() {
        let cfg = enabled_secret_cfg();
        let mut headers = HeaderMap::new();
        headers.insert("sec-websocket-protocol", "bearer.secret".parse().unwrap());
        match admit_node_ws(&cfg, &make_pairing(false), &headers, None, loopback_peer()) {
            NodeWsAdmission::Typed {
                status,
                code: NodeErrorCode::ProtocolUnsupported,
            } => assert_eq!(status, StatusCode::BAD_REQUEST),
            other => panic!("bearer-only subprotocol must not upgrade, got {other:?}"),
        }
    }

    #[test]
    fn nodes_v2_offer_with_bearer_token_is_admitted() {
        let cfg = enabled_secret_cfg();
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            format!("{WS_NODES_V2}, bearer.secret").parse().unwrap(),
        );
        assert_eq!(
            admit_node_ws(&cfg, &make_pairing(false), &headers, None, loopback_peer(),),
            NodeWsAdmission::Ok
        );
    }

    #[test]
    fn nodes_hello_minor_empty_intersection_closes_with_version_mismatch() {
        let outcome = handshake_first_frame(
            r#"{"type":"hello","protocol_versions":["2.1","v1"]}"#,
            &test_conn(),
        );
        match outcome {
            NodeHandshakeOutcome::Reject {
                frame:
                    GatewayToNode::Error {
                        code: NodeErrorCode::VersionMismatch,
                        retryable,
                        ..
                    },
                close_reason,
            } => {
                assert!(!retryable);
                assert_eq!(close_reason, "version_mismatch");
            }
            other => panic!("expected version_mismatch, got {other:?}"),
        }
    }

    #[test]
    fn nodes_non_loopback_is_fail_closed() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        match admit_node_ws(&cfg, &make_pairing(false), &headers, None, remote_peer()) {
            NodeWsAdmission::Typed {
                status,
                code: NodeErrorCode::LoopbackRequired,
            } => assert_eq!(status, StatusCode::FORBIDDEN),
            other => panic!("non-loopback must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn nodes_ipv6_loopback_is_admitted() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        let peer = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 40000));
        assert!(peer_is_loopback(peer));
        assert_eq!(
            admit_node_ws(&cfg, &make_pairing(false), &headers, None, peer),
            NodeWsAdmission::Ok
        );
    }

    #[test]
    fn nodes_v1_register_frame_is_typed_rejected() {
        let json = r#"{"type":"register","node_id":"phone-1","capabilities":[{"name":"camera.snap","description":"Take a photo"}]}"#;
        let outcome = handshake_first_frame(json, &test_conn());
        match outcome {
            NodeHandshakeOutcome::Reject {
                frame:
                    GatewayToNode::Error {
                        code: NodeErrorCode::ProtocolUnsupported,
                        retryable,
                        ..
                    },
                close_reason,
            } => {
                assert!(!retryable);
                assert_eq!(close_reason, "protocol_unsupported");
            }
            other => panic!("v1 Register must be rejected, got {other:?}"),
        }
        assert!(inbound_is_v1_register(json));
    }

    #[test]
    fn nodes_disabled_still_404_before_loopback_gate() {
        let cfg = NodesConfig {
            enabled: false,
            ..NodesConfig::default()
        };
        match admit_node_ws(
            &cfg,
            &make_pairing(false),
            &empty_headers(),
            None,
            remote_peer(),
        ) {
            NodeWsAdmission::Legacy {
                status: StatusCode::NOT_FOUND,
                ..
            } => {}
            other => panic!("disabled nodes stay 404, got {other:?}"),
        }
    }

    #[test]
    fn nodes_loopback_no_auth_still_503() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: None,
            ..NodesConfig::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        match admit_node_ws(&cfg, &make_pairing(false), &headers, None, loopback_peer()) {
            NodeWsAdmission::Legacy {
                status: StatusCode::SERVICE_UNAVAILABLE,
                ..
            } => {}
            other => panic!("no-auth posture must stay 503, got {other:?}"),
        }
    }

    #[test]
    fn nodes_connection_id_and_generation_are_minted_per_socket() {
        let registry = NodeRegistry::new(10);
        let a = registry.mint_connection();
        let b = registry.mint_connection();
        assert_ne!(a.connection_id, b.connection_id);
        assert_eq!(a.generation, 1);
        assert_eq!(b.generation, 2);
        let result = NodeToGateway::Result {
            call_id: "call-1".into(),
            connection_id: a.connection_id.clone(),
            success: true,
            output: "ok".into(),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"type\":\"result\""));
        assert!(json.contains(&format!("\"connection_id\":\"{}\"", a.connection_id)));
    }

    #[test]
    fn nodes_v2_invoke_grant_proof_literals() {
        let envelope = GatewayToNode::Invoke {
            call_id: "call-1".into(),
            connection_id: "conn-1".into(),
            cap: "system.notify".into(),
            cap_revision: 1,
            args: serde_json::json!({}),
            args_digest: "d".into(),
            deadline: "2026-08-13T00:00:00Z".into(),
            grant_proof: Some(GrantProof::Envelope {
                grant: serde_json::json!({"action":"system.notify"}),
                signature: "sig".into(),
                key_id: "k1".into(),
            }),
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["type"], "invoke");
        assert_eq!(json["grant_proof"]["kind"], "envelope");
        let handle = GrantProof::IntrospectHandle {
            grant_id: "g1".into(),
            nonce: "n1".into(),
        };
        let json = serde_json::to_value(&handle).unwrap();
        assert_eq!(json["kind"], "introspect_handle");
    }

    fn enabled_secret_cfg() -> NodesConfig {
        NodesConfig {
            enabled: true,
            auth_token: Some("secret".into()),
            ..NodesConfig::default()
        }
    }

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 40000))
    }

    fn remote_peer() -> SocketAddr {
        SocketAddr::from(([203, 0, 113, 50], 40000))
    }

    fn test_conn() -> NodeConnection {
        NodeConnection {
            connection_id: "conn-test".into(),
            generation: 7,
        }
    }

    #[test]
    fn extract_node_ws_token_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer node_tok_123".parse().unwrap());
        assert_eq!(extract_node_ws_token(&headers, None), Some("node_tok_123"));
    }

    #[test]
    fn extract_node_ws_token_from_query() {
        let headers = HeaderMap::new();
        assert_eq!(
            extract_node_ws_token(&headers, Some("node_tok_456")),
            Some("node_tok_456")
        );
    }

    #[test]
    fn extract_node_ws_token_none_when_empty() {
        let headers = HeaderMap::new();
        assert_eq!(extract_node_ws_token(&headers, None), None);
    }

    /// Regression for non-constant-time `nodes.auth_token` comparison.
    ///
    /// The old code used `if token != expected_token`, which short-circuits
    /// on the first differing byte and leaks the configured `nodes.auth_token`
    /// one byte at a time via response timing. The fix routes through
    /// `constant_time_eq` (from `zeroclaw_config::pairing`) and trims both
    /// sides — matching the canonical pairing pattern at
    /// `crates/zeroclaw-config/src/pairing.rs:139`.
    ///
    /// We can't measure wall-clock timing in a unit test, but we can lock
    /// in the behavior contract: trim-normalized inputs match, wrong
    /// tokens still reject. A future refactor that drops the
    /// `constant_time_eq` call (e.g., back to `==`) would still pass the
    /// "wrong token rejected" half — the trim half is what proves the new
    /// shape is in place.
    #[test]
    fn nodes_auth_token_compare_uses_constant_time_eq_and_trims() {
        let cfg = NodesConfig {
            enabled: true,
            auth_token: Some("node-secret-token".into()),
            ..NodesConfig::default()
        };

        // Whitespace-padded Authorization header still passes
        // (trim-normalized on both sides, per the canonical pairing pattern).
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer   node-secret-token   ".parse().unwrap(),
        );
        assert_eq!(
            check_node_auth(&cfg, &make_pairing(false), &headers, None),
            None,
            "auth_token comparison must trim both sides (canonical pairing pattern)"
        );

        // Wrong token via Authorization header is rejected.
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong-token".parse().unwrap());
        assert_eq!(
            check_node_auth(&cfg, &make_pairing(false), &headers, None).map(|(s, _)| s),
            Some(StatusCode::UNAUTHORIZED)
        );

        // Wrong token via `?token=` query parameter is also rejected.
        let empty_headers = HeaderMap::new();
        assert_eq!(
            check_node_auth(
                &cfg,
                &make_pairing(false),
                &empty_headers,
                Some("wrong-token"),
            )
            .map(|(s, _)| s),
            Some(StatusCode::UNAUTHORIZED)
        );

        // Correct token via `?token=` (with no trim ambiguity) passes.
        assert_eq!(
            check_node_auth(
                &cfg,
                &make_pairing(false),
                &empty_headers,
                Some("node-secret-token"),
            ),
            None
        );
    }
}
