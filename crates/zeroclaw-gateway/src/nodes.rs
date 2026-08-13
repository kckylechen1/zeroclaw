//! WebSocket endpoint for dynamic node discovery and capability advertisement.
//!
//! `generation` is a process-local monotonic counter minted per socket
//! (first connection is 1; a process restart resets it to 0). Tear-down of a
//! live socket must key on `connection_id`. Invocation-lifecycle work must
//! not treat `generation` as a durable identity on its own.

use super::AppState;
use crate::device_identity::{
    DeviceIdentityStore, PendingChallenge, admit_live_caps, auth_message, verify_auth_signature,
};
use axum::{
    Json,
    extract::{
        ConnectInfo, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
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

/// Hello must arrive within this window after the v2 upgrade.
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
const HELLO_MAX_BYTES: usize = 64 * 1024;
const MAX_PROTOCOL_VERSIONS: usize = 16;
const MAX_IDENTITY_FIELD_BYTES: usize = 256;

const AUTH_DEADLINE: Duration = Duration::from_secs(10);
const WS_MAX_MESSAGE_SIZE: usize = HELLO_MAX_BYTES;
const MAX_AUTH_SIGNATURE_BYTES: usize = 256;
const MAX_ADVERTISE_CAPS: usize = 16;
const MAX_CAP_NAME_BYTES: usize = 256;

const NODES_V2_NON_LOOPBACK_LISTEN_WARN: &str =
    "nodes v2 on a non-loopback listen rejects loopback TCP peers as a closed surface";

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
///
/// `generation` is not durable: it is monotonic only inside this process and
/// resets on restart. Invocation lifecycle must not key teardown on
/// `generation` alone; `connection_id` is the generation token that survives
/// a counter wrap or a new process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConnection {
    pub connection_id: String,
    pub generation: u64,
}

struct LiveSocket {
    connection_id: String,
    device_id: Option<String>,
    key_fingerprint: Option<String>,
    close_tx: tokio::sync::watch::Sender<bool>,
    widen_refusals: u32,
}

/// Registry of all connected nodes and their capabilities.
#[derive(Clone)]
pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
    max_nodes: usize,
    next_generation: Arc<AtomicU64>,
    /// Canonical listen IP from `TcpListener::local_addr` after bind.
    listen_addr: IpAddr,
    identities: DeviceIdentityStore,
    pending_challenges: Arc<RwLock<HashMap<String, PendingChallenge>>>,
    live: Arc<RwLock<HashMap<String, LiveSocket>>>,
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new(0)
    }
}

impl NodeRegistry {
    /// Create a new registry with the given capacity limit.
    ///
    /// Listen defaults to loopback so unit tests of the registry itself stay
    /// on the unverified bearer path.
    pub fn new(max_nodes: usize) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            max_nodes,
            next_generation: Arc::new(AtomicU64::new(0)),
            listen_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            identities: DeviceIdentityStore::memory_with_capacity(max_nodes.max(1)),
            pending_challenges: Arc::new(RwLock::new(HashMap::new())),
            live: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record the socket the gateway actually bound. Source of truth is
    /// `listener.local_addr()`, not the pre-bind config host string.
    #[must_use]
    pub fn with_listen_addr(mut self, listen_addr: IpAddr) -> Self {
        self.listen_addr = listen_addr;
        self
    }

    #[must_use]
    pub fn with_identities(mut self, identities: DeviceIdentityStore) -> Self {
        self.identities = identities;
        self
    }

    #[must_use]
    pub fn identities(&self) -> &DeviceIdentityStore {
        &self.identities
    }

    #[must_use]
    pub fn listen_addr(&self) -> IpAddr {
        self.listen_addr
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

    fn sweep_challenges(&self) {
        self.pending_challenges
            .write()
            .retain(|_, challenge| !challenge.expired());
    }

    /// Reserve a pre-auth slot counted against `max_nodes`.
    pub fn try_reserve(&self) -> Option<(NodeConnection, tokio::sync::watch::Receiver<bool>)> {
        self.sweep_challenges();
        let mut live = self.live.write();
        if live.len() >= self.max_nodes {
            return None;
        }
        let conn = self.mint_connection();
        let (close_tx, close_rx) = tokio::sync::watch::channel(false);
        live.insert(
            conn.connection_id.clone(),
            LiveSocket {
                connection_id: conn.connection_id.clone(),
                device_id: None,
                key_fingerprint: None,
                close_tx,
                widen_refusals: 0,
            },
        );
        Some((conn, close_rx))
    }

    pub fn begin_challenge(
        &self,
        conn: &NodeConnection,
        device_id: String,
        key_fingerprint: String,
    ) -> Result<PendingChallenge, crate::device_identity::IdentityError> {
        self.sweep_challenges();
        let challenge = PendingChallenge::issue(device_id, key_fingerprint)?;
        let mut pending = self.pending_challenges.write();
        if pending.len() >= self.max_nodes {
            return Err(crate::device_identity::IdentityError::Capacity);
        }
        pending.insert(conn.connection_id.clone(), challenge.clone());
        Ok(challenge)
    }

    pub fn take_challenge(&self, connection_id: &str) -> Option<PendingChallenge> {
        self.pending_challenges.write().remove(connection_id)
    }

    pub fn bind_identity(&self, connection_id: &str, device_id: String, key_fingerprint: String) {
        if let Some(socket) = self.live.write().get_mut(connection_id) {
            socket.device_id = Some(device_id);
            socket.key_fingerprint = Some(key_fingerprint);
        }
    }

    pub fn bound_identity(&self, connection_id: &str) -> Option<(String, String)> {
        let live = self.live.read();
        let socket = live.get(connection_id)?;
        Some((socket.device_id.clone()?, socket.key_fingerprint.clone()?))
    }

    pub fn detach_socket(&self, connection_id: &str) {
        self.live.write().remove(connection_id);
        self.pending_challenges.write().remove(connection_id);
    }

    /// Revoke the identity and tear live sockets for that device.
    pub fn revoke_device(
        &self,
        device_id: &str,
    ) -> Result<Vec<String>, crate::device_identity::IdentityError> {
        if !self.identities.revoke(device_id)? {
            return Ok(Vec::new());
        }
        let mut torn = Vec::new();
        let mut live = self.live.write();
        live.retain(|_, socket| {
            if socket.device_id.as_deref() == Some(device_id) {
                let _ = socket.close_tx.send(true);
                torn.push(socket.connection_id.clone());
                false
            } else {
                true
            }
        });
        Ok(torn)
    }

    #[must_use]
    pub fn live_connection_ids(&self) -> Vec<String> {
        self.live.read().keys().cloned().collect()
    }

    #[must_use]
    pub fn pending_challenge_count(&self) -> usize {
        self.sweep_challenges();
        self.pending_challenges.read().len()
    }

    pub fn admit_advertised_caps(
        &self,
        device_id: &str,
        key_fingerprint: &str,
        advertised: &[String],
    ) -> Result<Vec<String>, crate::device_identity::IdentityError> {
        let Some(identity) = self.identities.active_identity(device_id, key_fingerprint) else {
            return Err(crate::device_identity::IdentityError::IdentityRejected);
        };
        admit_live_caps(advertised, &identity.capability_ceiling)
    }

    fn note_widen_refusal(&self, connection_id: &str) -> u32 {
        let mut live = self.live.write();
        if let Some(socket) = live.get_mut(connection_id) {
            socket.widen_refusals = socket.widen_refusals.saturating_add(1);
            return socket.widen_refusals;
        }
        0
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

fn listen_is_loopback(listen: IpAddr) -> bool {
    listen.is_loopback()
}

/// True only when both the bound listen address and the TCP peer are loopback.
/// A same-host reverse proxy makes `ConnectInfo` loopback while the gateway
/// listens on `0.0.0.0` / `::`; that combination is never trusted as local.
fn trusted_local(listen: IpAddr, peer: SocketAddr) -> bool {
    listen_is_loopback(listen) && peer_is_loopback(peer)
}

fn reverse_proxy_disguise(listen: IpAddr, peer: SocketAddr) -> bool {
    !listen_is_loopback(listen) && peer_is_loopback(peer)
}

/// WARN copy when nodes are enabled but the gateway did not bind loopback.
#[must_use]
pub(crate) fn nodes_v2_non_loopback_listen_warning(
    enabled: bool,
    listen: IpAddr,
) -> Option<&'static str> {
    if enabled && !listen_is_loopback(listen) {
        Some(NODES_V2_NON_LOOPBACK_LISTEN_WARN)
    } else {
        None
    }
}

fn closed_surface() -> NodeWsAdmission {
    NodeWsAdmission::Legacy {
        status: StatusCode::NOT_FOUND,
        body: NODES_DISABLED_MSG,
    }
}

/// HTTP admission for `/ws/nodes`.
///
/// Dual loopback (listen + peer from `listener.local_addr()`) is the trusted
/// local foundation and keeps the typed 404/503/401/400 surface. A loopback
/// peer on a non-loopback listen is treated as a same-host reverse proxy and
/// stays on the closed 404 surface. A true remote peer without a valid bearer
/// (or without v2) receives the same 404 bytes as `enabled=false`. A remote
/// client that presents a valid bearer and v2 is upgraded; identity is proven
/// only in-band after Hello.
fn admit_node_ws(
    nodes_config: &zeroclaw_config::schema::NodesConfig,
    pairing: &PairingGuard,
    headers: &HeaderMap,
    query_token: Option<&str>,
    peer: SocketAddr,
    listen: IpAddr,
) -> NodeWsAdmission {
    if reverse_proxy_disguise(listen, peer) {
        return closed_surface();
    }
    if trusted_local(listen, peer) {
        if !nodes_config.enabled {
            return closed_surface();
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
        return NodeWsAdmission::Ok;
    }
    if !nodes_config.enabled
        || check_node_auth(nodes_config, pairing, headers, query_token).is_some()
        || !client_offers_subprotocol(headers, WS_NODES_V2)
    {
        return closed_surface();
    }
    NodeWsAdmission::Ok
}

#[derive(Debug, Clone, PartialEq)]
enum ParsedHello {
    Ready {
        protocol_version: String,
        device_id: Option<String>,
        key_fingerprint: Option<String>,
    },
    Reject {
        frame: GatewayToNode,
        close_reason: &'static str,
    },
}

#[cfg(test)]
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

fn identity_field_too_long(value: Option<&str>) -> bool {
    value.is_some_and(|s| s.len() > MAX_IDENTITY_FIELD_BYTES)
}

fn parse_hello_frame(text: &str) -> ParsedHello {
    if text.len() > HELLO_MAX_BYTES {
        return parsed_hello_reject(NodeErrorCode::ProtocolUnsupported);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return parsed_hello_reject(NodeErrorCode::ProtocolUnsupported);
    };
    if is_v1_register_frame(&value) {
        return parsed_hello_reject(NodeErrorCode::ProtocolUnsupported);
    }
    let Ok(frame) = serde_json::from_value::<NodeToGateway>(value) else {
        return parsed_hello_reject(NodeErrorCode::ProtocolUnsupported);
    };
    match frame {
        NodeToGateway::Hello {
            protocol_versions,
            device_id,
            key_fingerprint,
        } => {
            if protocol_versions.len() > MAX_PROTOCOL_VERSIONS
                || identity_field_too_long(device_id.as_deref())
                || identity_field_too_long(key_fingerprint.as_deref())
            {
                return parsed_hello_reject(NodeErrorCode::ProtocolUnsupported);
            }
            match negotiate_v2_minor(&protocol_versions) {
                Some(version) => ParsedHello::Ready {
                    protocol_version: version.to_string(),
                    device_id,
                    key_fingerprint,
                },
                None => parsed_hello_reject(NodeErrorCode::VersionMismatch),
            }
        }
        _ => parsed_hello_reject(NodeErrorCode::ProtocolUnsupported),
    }
}

fn parsed_hello_reject(code: NodeErrorCode) -> ParsedHello {
    ParsedHello::Reject {
        frame: protocol_error_frame(code),
        close_reason: code.as_str(),
    }
}

fn parse_auth_frame(text: &str) -> Result<(String, u64), NodeErrorCode> {
    if text.len() > HELLO_MAX_BYTES {
        return Err(NodeErrorCode::ProtocolUnsupported);
    }
    let Ok(NodeToGateway::Auth {
        signature,
        identity_epoch,
    }) = serde_json::from_str::<NodeToGateway>(text)
    else {
        return Err(NodeErrorCode::IdentityRejected);
    };
    if signature.len() > MAX_AUTH_SIGNATURE_BYTES {
        return Err(NodeErrorCode::ProtocolUnsupported);
    }
    Ok((signature, identity_epoch))
}

fn identity_required(
    listen: IpAddr,
    peer: SocketAddr,
    device_id: Option<&str>,
    key_fingerprint: Option<&str>,
) -> bool {
    !trusted_local(listen, peer) || device_id.is_some() || key_fingerprint.is_some()
}

fn hello_ack(conn: &NodeConnection, protocol_version: String) -> GatewayToNode {
    GatewayToNode::HelloAck {
        protocol_version,
        connection_id: conn.connection_id.clone(),
        generation: conn.generation,
    }
}

/// First in-band frame after a v2 upgrade: Hello with overlapping minors, or reject.
/// Loopback peers without identity fields still receive HelloAck (bearer test path).
#[cfg(test)]
fn handshake_first_frame(text: &str, conn: &NodeConnection) -> NodeHandshakeOutcome {
    match parse_hello_frame(text) {
        ParsedHello::Ready {
            protocol_version, ..
        } => NodeHandshakeOutcome::Ack(hello_ack(conn, protocol_version)),
        ParsedHello::Reject {
            frame,
            close_reason,
        } => NodeHandshakeOutcome::Reject {
            frame,
            close_reason,
        },
    }
}

/// RFC 8032 test-vector public key. Used only so unknown/revoked devices
/// still pay a full Ed25519 verify before the unified reject.
const DUMMY_VERIFY_PUBLIC_KEY_HEX: &str =
    "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

fn verify_node_auth(
    registry: &NodeRegistry,
    conn: &NodeConnection,
    signature: &str,
    identity_epoch: u64,
) -> Result<String, NodeErrorCode> {
    let challenge = registry.take_challenge(&conn.connection_id);
    let challenge_ok = challenge.as_ref().is_some_and(|c| !c.expired());
    let challenge = challenge.unwrap_or_else(|| PendingChallenge {
        nonce: "00".repeat(32),
        expires_at: Utc::now(),
        device_id: "unknown".into(),
        key_fingerprint: "unknown".into(),
    });
    let identity = registry
        .identities()
        .active_identity(&challenge.device_id, &challenge.key_fingerprint);
    let epoch_ok = identity
        .as_ref()
        .is_some_and(|id| id.identity_epoch == identity_epoch);
    let public_key = identity
        .as_ref()
        .map(|id| id.public_key.as_str())
        .unwrap_or(DUMMY_VERIFY_PUBLIC_KEY_HEX);
    let message = auth_message(
        &challenge.nonce,
        &challenge.device_id,
        &challenge.key_fingerprint,
        identity_epoch,
    );
    let verified = verify_auth_signature(public_key, &message, signature).is_ok();
    match identity {
        Some(identity) if challenge_ok && epoch_ok && verified => Ok(identity.device_id),
        _ => Err(NodeErrorCode::IdentityRejected),
    }
}

fn apply_advertise(
    registry: &NodeRegistry,
    conn: &NodeConnection,
    caps: &[String],
    cap_revision: u64,
) -> (GatewayToNode, bool) {
    let Some((device_id, key_fingerprint)) = registry.bound_identity(&conn.connection_id) else {
        return (protocol_error_frame(NodeErrorCode::IdentityRejected), true);
    };
    match registry.admit_advertised_caps(&device_id, &key_fingerprint, caps) {
        Ok(admitted) => (
            GatewayToNode::Admitted {
                caps: admitted,
                cap_revision,
            },
            false,
        ),
        Err(crate::device_identity::IdentityError::WidenRefused) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "device_id": device_id,
                        "event": "capability_widen"
                    })),
                "node advertised a capability outside the approved ceiling"
            );
            let strikes = registry.note_widen_refusal(&conn.connection_id);
            (
                protocol_error_frame(NodeErrorCode::CapabilityWiden),
                strikes >= 2,
            )
        }
        Err(_) => (protocol_error_frame(NodeErrorCode::IdentityRejected), true),
    }
}

fn process_post_handshake_text(
    registry: &NodeRegistry,
    conn: &NodeConnection,
    text: &str,
) -> Option<(GatewayToNode, bool)> {
    if text.len() > HELLO_MAX_BYTES {
        return Some((
            protocol_error_frame(NodeErrorCode::ProtocolUnsupported),
            true,
        ));
    }
    if inbound_is_v1_register(text) {
        return Some((
            protocol_error_frame(NodeErrorCode::ProtocolUnsupported),
            true,
        ));
    }
    let Ok(frame) = serde_json::from_str::<NodeToGateway>(text) else {
        return None;
    };
    match frame {
        NodeToGateway::Advertise { caps, cap_revision } => {
            if caps.len() > MAX_ADVERTISE_CAPS
                || caps.iter().any(|cap| cap.len() > MAX_CAP_NAME_BYTES)
            {
                return Some((
                    protocol_error_frame(NodeErrorCode::ProtocolUnsupported),
                    true,
                ));
            }
            Some(apply_advertise(registry, conn, &caps, cap_revision))
        }
        NodeToGateway::Auth { signature, .. } => {
            if signature.len() > MAX_AUTH_SIGNATURE_BYTES {
                return Some((
                    protocol_error_frame(NodeErrorCode::ProtocolUnsupported),
                    true,
                ));
            }
            None
        }
        _ => None,
    }
}

#[derive(Debug)]
enum FirstWsFrame {
    Text(String),
    Control,
    Closed,
    Reject,
}

fn classify_first_ws_frame(msg: Result<Message, axum::Error>) -> FirstWsFrame {
    match msg {
        Ok(Message::Text(text)) => FirstWsFrame::Text(text.to_string()),
        Ok(Message::Ping(_) | Message::Pong(_)) => FirstWsFrame::Control,
        Ok(Message::Close(_)) | Err(_) => FirstWsFrame::Closed,
        Ok(Message::Binary(_)) => FirstWsFrame::Reject,
    }
}

async fn recv_hello_text(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Option<Result<String, ()>> {
    loop {
        match receiver.next().await {
            None => return None,
            Some(msg) => match classify_first_ws_frame(msg) {
                FirstWsFrame::Text(text) => return Some(Ok(text)),
                FirstWsFrame::Control => continue,
                FirstWsFrame::Closed => return None,
                FirstWsFrame::Reject => return Some(Err(())),
            },
        }
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
        state.node_registry.listen_addr(),
    ) {
        NodeWsAdmission::Ok => {}
        NodeWsAdmission::Legacy { status, body } => return (status, body).into_response(),
        NodeWsAdmission::Typed { status, code } => {
            return (status, Json(NodeProtocolReject { code })).into_response();
        }
    }

    let registry = state.node_registry.clone();
    let listen = registry.listen_addr();
    ws.protocols([WS_NODES_V2])
        .max_message_size(WS_MAX_MESSAGE_SIZE)
        .max_frame_size(WS_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_node_socket(socket, registry, peer, listen))
        .into_response()
}

struct LiveSocketGuard {
    registry: Arc<NodeRegistry>,
    connection_id: String,
}

impl Drop for LiveSocketGuard {
    fn drop(&mut self) {
        self.registry.detach_socket(&self.connection_id);
    }
}

async fn handle_node_socket(
    socket: WebSocket,
    registry: Arc<NodeRegistry>,
    peer: SocketAddr,
    listen: IpAddr,
) {
    let (mut sender, mut receiver) = socket.split();
    let Some((conn, mut close_rx)) = registry.try_reserve() else {
        let _ = send_json(
            &mut sender,
            &protocol_error_frame(NodeErrorCode::IdentityRejected),
        )
        .await;
        let _ = close_protocol(&mut sender, NodeErrorCode::IdentityRejected.as_str()).await;
        return;
    };
    let _guard = LiveSocketGuard {
        registry: registry.clone(),
        connection_id: conn.connection_id.clone(),
    };

    let first_text = match tokio::time::timeout(HELLO_DEADLINE, recv_hello_text(&mut receiver))
        .await
    {
        Ok(Some(Ok(text))) => text,
        Ok(Some(Err(()))) | Err(_) => {
            let _ = send_json(
                &mut sender,
                &protocol_error_frame(NodeErrorCode::ProtocolUnsupported),
            )
            .await;
            let _ = close_protocol(&mut sender, NodeErrorCode::ProtocolUnsupported.as_str()).await;
            return;
        }
        Ok(None) => return,
    };

    let (protocol_version, device_id, key_fingerprint) = match parse_hello_frame(&first_text) {
        ParsedHello::Ready {
            protocol_version,
            device_id,
            key_fingerprint,
        } => (protocol_version, device_id, key_fingerprint),
        ParsedHello::Reject {
            frame,
            close_reason,
        } => {
            let _ = send_json(&mut sender, &frame).await;
            let _ = close_protocol(&mut sender, close_reason).await;
            return;
        }
    };

    if identity_required(
        listen,
        peer,
        device_id.as_deref(),
        key_fingerprint.as_deref(),
    ) {
        let Some(device_id) = device_id else {
            let code = if key_fingerprint.is_none() {
                NodeErrorCode::LoopbackRequired
            } else {
                NodeErrorCode::IdentityRejected
            };
            let _ = send_json(&mut sender, &protocol_error_frame(code)).await;
            let _ = close_protocol(&mut sender, code.as_str()).await;
            return;
        };
        let Some(key_fingerprint) = key_fingerprint else {
            let code = NodeErrorCode::IdentityRejected;
            let _ = send_json(&mut sender, &protocol_error_frame(code)).await;
            let _ = close_protocol(&mut sender, code.as_str()).await;
            return;
        };
        let challenge =
            match registry.begin_challenge(&conn, device_id.clone(), key_fingerprint.clone()) {
                Ok(challenge) => challenge,
                Err(_) => {
                    let code = NodeErrorCode::IdentityRejected;
                    let _ = send_json(&mut sender, &protocol_error_frame(code)).await;
                    let _ = close_protocol(&mut sender, code.as_str()).await;
                    return;
                }
            };
        let frame = GatewayToNode::Challenge {
            nonce: challenge.nonce.clone(),
            expires_at: challenge.expires_at.to_rfc3339(),
        };
        if send_json(&mut sender, &frame).await.is_err() {
            return;
        }
        let auth_text =
            match tokio::time::timeout(AUTH_DEADLINE, recv_hello_text(&mut receiver)).await {
                Ok(Some(Ok(text))) => text,
                Ok(Some(Err(()))) | Err(_) => {
                    let code = NodeErrorCode::IdentityRejected;
                    let _ = send_json(&mut sender, &protocol_error_frame(code)).await;
                    let _ = close_protocol(&mut sender, code.as_str()).await;
                    return;
                }
                Ok(None) => return,
            };
        let (signature, identity_epoch) = match parse_auth_frame(&auth_text) {
            Ok(parsed) => parsed,
            Err(code) => {
                let _ = send_json(&mut sender, &protocol_error_frame(code)).await;
                let _ = close_protocol(&mut sender, code.as_str()).await;
                return;
            }
        };
        match verify_node_auth(&registry, &conn, &signature, identity_epoch) {
            Ok(verified_id) => {
                registry.bind_identity(&conn.connection_id, verified_id, key_fingerprint);
            }
            Err(code) => {
                let _ = send_json(&mut sender, &protocol_error_frame(code)).await;
                let _ = close_protocol(&mut sender, code.as_str()).await;
                return;
            }
        }
    }

    let ack = hello_ack(&conn, protocol_version);
    if send_json(&mut sender, &ack).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            changed = close_rx.changed() => {
                if changed.is_err() || *close_rx.borrow() {
                    let _ = close_protocol(&mut sender, NodeErrorCode::IdentityRejected.as_str()).await;
                    return;
                }
            }
            msg = receiver.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                if let Some((frame, close_after)) = process_post_handshake_text(&registry, &conn, &text) {
                    let close_reason = match &frame {
                        GatewayToNode::Error { code, .. } if close_after => code.as_str(),
                        _ => NodeErrorCode::ProtocolUnsupported.as_str(),
                    };
                    let _ = send_json(&mut sender, &frame).await;
                    if close_after {
                        let _ = close_protocol(&mut sender, close_reason).await;
                        return;
                    }
                }
            }
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
        match admit_node_ws(
            &cfg,
            &make_pairing(false),
            &headers,
            None,
            loopback_peer(),
            loopback_listen(),
        ) {
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
        match admit_node_ws(
            &cfg,
            &make_pairing(false),
            &headers,
            None,
            loopback_peer(),
            loopback_listen(),
        ) {
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
        match admit_node_ws(
            &cfg,
            &make_pairing(false),
            &headers,
            None,
            loopback_peer(),
            loopback_listen(),
        ) {
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
            admit_node_ws(
                &cfg,
                &make_pairing(false),
                &headers,
                None,
                loopback_peer(),
                loopback_listen(),
            ),
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
    fn nodes_non_loopback_peer_is_admitted_at_http() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        assert_eq!(
            admit_node_ws(
                &cfg,
                &make_pairing(false),
                &headers,
                None,
                remote_peer(),
                loopback_listen(),
            ),
            NodeWsAdmission::Ok
        );
    }

    #[test]
    fn remote_http_matches_disabled_bytes_without_valid_bearer() {
        let enabled = enabled_secret_cfg();
        let disabled = NodesConfig {
            enabled: false,
            ..NodesConfig::default()
        };
        let mut good = bearer_headers("secret");
        good.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        let mut bad = bearer_headers("wrong");
        bad.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        let disabled_bytes = admission_http_bytes(&admit_node_ws(
            &disabled,
            &make_pairing(false),
            &good,
            None,
            remote_peer(),
            unspecified_listen(),
        ));
        assert_eq!(disabled_bytes, disabled_surface_http_bytes());
        assert_eq!(
            admission_http_bytes(&admit_node_ws(
                &enabled,
                &make_pairing(false),
                &bad,
                None,
                remote_peer(),
                unspecified_listen(),
            )),
            disabled_bytes
        );
        assert_eq!(
            admission_http_bytes(&admit_node_ws(
                &enabled,
                &make_pairing(false),
                &bearer_headers("secret"),
                None,
                remote_peer(),
                unspecified_listen(),
            )),
            disabled_bytes
        );
        let unpaired = NodesConfig {
            enabled: true,
            auth_token: None,
            ..NodesConfig::default()
        };
        assert_eq!(
            admission_http_bytes(&admit_node_ws(
                &unpaired,
                &make_pairing(false),
                &good,
                None,
                remote_peer(),
                unspecified_listen(),
            )),
            disabled_bytes,
            "enabled-unpaired remote HTTP must match disabled bytes"
        );
    }

    #[test]
    fn nodes_ipv6_loopback_is_admitted() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        let peer = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 40000));
        assert!(peer_is_loopback(peer));
        assert_eq!(
            admit_node_ws(
                &cfg,
                &make_pairing(false),
                &headers,
                None,
                peer,
                loopback_listen(),
            ),
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
            loopback_listen(),
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
        match admit_node_ws(
            &cfg,
            &make_pairing(false),
            &headers,
            None,
            loopback_peer(),
            loopback_listen(),
        ) {
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

    fn loopback_listen() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    fn unspecified_listen() -> IpAddr {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    }

    fn admission_http_bytes(admission: &NodeWsAdmission) -> (StatusCode, &'static str) {
        match admission {
            NodeWsAdmission::Legacy { status, body } => (*status, *body),
            other => panic!("expected closed-surface HTTP body, got {other:?}"),
        }
    }

    fn disabled_surface_http_bytes() -> (StatusCode, &'static str) {
        (StatusCode::NOT_FOUND, NODES_DISABLED_MSG)
    }

    fn test_conn() -> NodeConnection {
        NodeConnection {
            connection_id: "conn-test".into(),
            generation: 7,
        }
    }

    #[test]
    fn nodes_unspecified_listen_rejects_loopback_peer() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        let rejected = admit_node_ws(
            &cfg,
            &make_pairing(false),
            &headers,
            None,
            loopback_peer(),
            unspecified_listen(),
        );
        assert_eq!(
            admission_http_bytes(&rejected),
            disabled_surface_http_bytes(),
            "0.0.0.0 listen + loopback peer must not be treated as local"
        );
    }

    #[test]
    fn nodes_unspecified_listen_admits_true_remote_peer_with_bearer() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        assert_eq!(
            admit_node_ws(
                &cfg,
                &make_pairing(false),
                &headers,
                None,
                remote_peer(),
                unspecified_listen(),
            ),
            NodeWsAdmission::Ok
        );
    }

    #[test]
    fn nodes_v2_warns_only_when_enabled_on_non_loopback_listen() {
        assert_eq!(
            nodes_v2_non_loopback_listen_warning(true, unspecified_listen()),
            Some(NODES_V2_NON_LOOPBACK_LISTEN_WARN)
        );
        assert_eq!(
            nodes_v2_non_loopback_listen_warning(true, loopback_listen()),
            None
        );
        assert_eq!(
            nodes_v2_non_loopback_listen_warning(false, unspecified_listen()),
            None
        );
    }

    #[test]
    fn nodes_loopback_listen_and_peer_are_admitted() {
        let cfg = enabled_secret_cfg();
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        assert_eq!(
            admit_node_ws(
                &cfg,
                &make_pairing(false),
                &headers,
                None,
                loopback_peer(),
                loopback_listen(),
            ),
            NodeWsAdmission::Ok
        );
    }

    #[test]
    fn nodes_disabled_404_bytes_unchanged() {
        let disabled_cfg = NodesConfig {
            enabled: false,
            ..NodesConfig::default()
        };
        let mut headers = bearer_headers("secret");
        headers.insert("sec-websocket-protocol", WS_NODES_V2.parse().unwrap());
        let disabled = admit_node_ws(
            &disabled_cfg,
            &make_pairing(false),
            &headers,
            None,
            remote_peer(),
            unspecified_listen(),
        );
        assert_eq!(
            admission_http_bytes(&disabled),
            disabled_surface_http_bytes()
        );
    }

    #[test]
    fn nodes_hello_rejects_oversize_message() {
        let oversized = format!(
            r#"{{"type":"hello","protocol_versions":["2.0"],"pad":"{}"}}"#,
            "x".repeat(HELLO_MAX_BYTES)
        );
        assert!(oversized.len() > HELLO_MAX_BYTES);
        match handshake_first_frame(&oversized, &test_conn()) {
            NodeHandshakeOutcome::Reject {
                frame:
                    GatewayToNode::Error {
                        code: NodeErrorCode::ProtocolUnsupported,
                        ..
                    },
                ..
            } => {}
            other => panic!("oversize Hello must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn nodes_hello_rejects_too_many_protocol_versions() {
        let versions: Vec<String> = (0..17).map(|i| format!("2.{i}")).collect();
        let frame = NodeToGateway::Hello {
            protocol_versions: versions,
            device_id: None,
            key_fingerprint: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        match handshake_first_frame(&json, &test_conn()) {
            NodeHandshakeOutcome::Reject {
                frame:
                    GatewayToNode::Error {
                        code: NodeErrorCode::ProtocolUnsupported,
                        ..
                    },
                ..
            } => {}
            other => panic!(">16 protocol_versions must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn nodes_hello_rejects_oversized_identity_fields() {
        let too_long = "f".repeat(MAX_IDENTITY_FIELD_BYTES + 1);
        let frame = NodeToGateway::Hello {
            protocol_versions: vec!["2.0".into()],
            device_id: Some(too_long.clone()),
            key_fingerprint: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        match handshake_first_frame(&json, &test_conn()) {
            NodeHandshakeOutcome::Reject {
                frame:
                    GatewayToNode::Error {
                        code: NodeErrorCode::ProtocolUnsupported,
                        ..
                    },
                ..
            } => {}
            other => panic!("oversize device_id must fail closed, got {other:?}"),
        }
        let frame = NodeToGateway::Hello {
            protocol_versions: vec!["2.0".into()],
            device_id: None,
            key_fingerprint: Some(too_long),
        };
        let json = serde_json::to_string(&frame).unwrap();
        match handshake_first_frame(&json, &test_conn()) {
            NodeHandshakeOutcome::Reject {
                frame:
                    GatewayToNode::Error {
                        code: NodeErrorCode::ProtocolUnsupported,
                        ..
                    },
                ..
            } => {}
            other => panic!("oversize key_fingerprint must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn nodes_ws_max_message_size_is_capped_like_hello() {
        assert_eq!(WS_MAX_MESSAGE_SIZE, HELLO_MAX_BYTES);
        assert_eq!(WS_MAX_MESSAGE_SIZE, 64 * 1024);
    }

    #[test]
    fn auth_frame_rejects_oversize_message_and_signature() {
        let oversized = format!(
            r#"{{"type":"auth","signature":"{}","identity_epoch":1}}"#,
            "a".repeat(HELLO_MAX_BYTES)
        );
        assert_eq!(
            parse_auth_frame(&oversized),
            Err(NodeErrorCode::ProtocolUnsupported)
        );
        let long_sig = NodeToGateway::Auth {
            signature: "a".repeat(MAX_AUTH_SIGNATURE_BYTES + 1),
            identity_epoch: 1,
        };
        let json = serde_json::to_string(&long_sig).unwrap();
        assert_eq!(
            parse_auth_frame(&json),
            Err(NodeErrorCode::ProtocolUnsupported)
        );
        assert_eq!(
            parse_auth_frame(r#"{"type":"hello","protocol_versions":["2.0"]}"#),
            Err(NodeErrorCode::IdentityRejected)
        );
    }

    #[test]
    fn advertise_frame_rejects_oversize_count_and_item() {
        let registry = NodeRegistry::new(8);
        let (_keys, identity) = enroll_test_device(&registry, vec!["system.notify".into()]);
        let (conn, _rx) = registry.try_reserve().expect("reserve");
        registry.bind_identity(
            &conn.connection_id,
            identity.device_id.clone(),
            identity.key_fingerprint.clone(),
        );
        let too_many: Vec<String> = (0..MAX_ADVERTISE_CAPS + 1)
            .map(|i| format!("cap.{i}"))
            .collect();
        let frame = NodeToGateway::Advertise {
            caps: too_many,
            cap_revision: 1,
        };
        let json = serde_json::to_string(&frame).unwrap();
        match process_post_handshake_text(&registry, &conn, &json) {
            Some((
                GatewayToNode::Error {
                    code: NodeErrorCode::ProtocolUnsupported,
                    ..
                },
                true,
            )) => {}
            other => panic!("too many caps must fail closed, got {other:?}"),
        }
        let long_name = NodeToGateway::Advertise {
            caps: vec!["n".repeat(MAX_CAP_NAME_BYTES + 1)],
            cap_revision: 1,
        };
        let json = serde_json::to_string(&long_name).unwrap();
        match process_post_handshake_text(&registry, &conn, &json) {
            Some((
                GatewayToNode::Error {
                    code: NodeErrorCode::ProtocolUnsupported,
                    ..
                },
                true,
            )) => {}
            other => panic!("oversize cap name must fail closed, got {other:?}"),
        }
        let oversized = format!(
            r#"{{"type":"advertise","caps":["system.notify"],"cap_revision":1,"pad":"{}"}}"#,
            "x".repeat(HELLO_MAX_BYTES)
        );
        match process_post_handshake_text(&registry, &conn, &oversized) {
            Some((
                GatewayToNode::Error {
                    code: NodeErrorCode::ProtocolUnsupported,
                    ..
                },
                true,
            )) => {}
            other => panic!("oversize advertise must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn nodes_first_frame_binary_is_rejected_ping_is_control() {
        match classify_first_ws_frame(Ok(Message::Binary(vec![1, 2, 3].into()))) {
            FirstWsFrame::Reject => {}
            other => panic!("binary must reject, got {other:?}"),
        }
        match classify_first_ws_frame(Ok(Message::Ping(vec![].into()))) {
            FirstWsFrame::Control => {}
            other => panic!("ping must be control, got {other:?}"),
        }
        match classify_first_ws_frame(Ok(Message::Text(
            r#"{"type":"hello","protocol_versions":["2.0"]}"#.into(),
        ))) {
            FirstWsFrame::Text(_) => {}
            other => panic!("text must be accepted, got {other:?}"),
        }
    }

    #[test]
    fn nodes_hello_deadline_is_ten_seconds() {
        assert_eq!(HELLO_DEADLINE, Duration::from_secs(10));
    }

    fn enroll_test_device(
        registry: &NodeRegistry,
        ceiling: Vec<String>,
    ) -> (
        crate::device_identity::DeviceKeyPair,
        zeroclaw_api::device_identity::DeviceIdentityV1,
    ) {
        let keys = crate::device_identity::DeviceKeyPair::generate().unwrap();
        let code = registry.identities().issue_pairing_code(ceiling).unwrap();
        let identity = registry
            .identities()
            .enroll(&code, keys.public_key_hex())
            .unwrap();
        (keys, identity)
    }

    fn sign_challenge(
        keys: &crate::device_identity::DeviceKeyPair,
        challenge: &crate::device_identity::PendingChallenge,
        epoch: u64,
    ) -> String {
        keys.sign(&auth_message(
            &challenge.nonce,
            &challenge.device_id,
            &challenge.key_fingerprint,
            epoch,
        ))
    }

    #[test]
    fn unpaired_non_loopback_requires_identity() {
        assert!(identity_required(
            loopback_listen(),
            remote_peer(),
            None,
            None
        ));
        assert!(!identity_required(
            loopback_listen(),
            loopback_peer(),
            None,
            None
        ));
        assert!(identity_required(
            unspecified_listen(),
            loopback_peer(),
            None,
            None
        ));
    }

    #[test]
    fn paired_signed_non_loopback_is_admitted() {
        let registry = NodeRegistry::new(8);
        let (keys, identity) = enroll_test_device(&registry, vec!["system.notify".into()]);
        let conn = registry.mint_connection();
        let challenge = registry
            .begin_challenge(
                &conn,
                identity.device_id.clone(),
                identity.key_fingerprint.clone(),
            )
            .unwrap();
        let signature = sign_challenge(&keys, &challenge, identity.identity_epoch);
        let device_id = verify_node_auth(&registry, &conn, &signature, identity.identity_epoch)
            .expect("signed challenge must admit a paired device");
        assert_eq!(device_id, identity.device_id);
        assert_eq!(
            registry
                .admit_advertised_caps(
                    &identity.device_id,
                    &identity.key_fingerprint,
                    &["system.notify".into()]
                )
                .unwrap(),
            ["system.notify"]
        );
        assert_eq!(
            registry.admit_advertised_caps(
                &identity.device_id,
                &identity.key_fingerprint,
                &["camera.snap".into()]
            ),
            Err(crate::device_identity::IdentityError::WidenRefused)
        );
    }

    #[test]
    fn unknown_device_and_bad_signature_share_identity_rejected() {
        let registry = NodeRegistry::new(8);
        let (keys, identity) = enroll_test_device(&registry, vec!["system.notify".into()]);
        let other = crate::device_identity::DeviceKeyPair::generate().unwrap();
        let conn_unknown = registry.mint_connection();
        let unknown = registry
            .begin_challenge(&conn_unknown, "missing".into(), "ffff".into())
            .unwrap();
        let unknown_sig = other.sign(&auth_message(&unknown.nonce, "missing", "ffff", 1));
        let unknown_err = verify_node_auth(&registry, &conn_unknown, &unknown_sig, 1)
            .expect_err("unknown device");
        let conn_bad = registry.mint_connection();
        let challenge = registry
            .begin_challenge(
                &conn_bad,
                identity.device_id.clone(),
                identity.key_fingerprint.clone(),
            )
            .unwrap();
        let bad_sig = sign_challenge(&other, &challenge, identity.identity_epoch);
        let bad_err = verify_node_auth(&registry, &conn_bad, &bad_sig, identity.identity_epoch)
            .expect_err("bad signature");
        assert_eq!(unknown_err, NodeErrorCode::IdentityRejected);
        assert_eq!(bad_err, NodeErrorCode::IdentityRejected);
        assert_eq!(unknown_err.as_str(), bad_err.as_str());
        let _ = keys;
    }

    #[test]
    fn replayed_challenge_is_rejected() {
        let registry = NodeRegistry::new(8);
        let (keys, identity) = enroll_test_device(&registry, vec!["system.notify".into()]);
        let conn = registry.mint_connection();
        let challenge = registry
            .begin_challenge(
                &conn,
                identity.device_id.clone(),
                identity.key_fingerprint.clone(),
            )
            .unwrap();
        let signature = sign_challenge(&keys, &challenge, identity.identity_epoch);
        assert!(verify_node_auth(&registry, &conn, &signature, identity.identity_epoch).is_ok());
        assert_eq!(
            verify_node_auth(&registry, &conn, &signature, identity.identity_epoch),
            Err(NodeErrorCode::IdentityRejected)
        );
    }

    #[test]
    fn revoke_tears_live_socket_and_rejects_reconnect() {
        let registry = NodeRegistry::new(8);
        let (keys, identity) = enroll_test_device(&registry, vec!["system.notify".into()]);
        let (conn, close_rx) = registry.try_reserve().expect("reserve");
        registry.bind_identity(
            &conn.connection_id,
            identity.device_id.clone(),
            identity.key_fingerprint.clone(),
        );
        let torn = registry
            .revoke_device(&identity.device_id)
            .expect("revoke persist");
        assert_eq!(torn, vec![conn.connection_id.clone()]);
        assert!(*close_rx.borrow());
        assert!(registry.live_connection_ids().is_empty());
        let reconnect = registry.mint_connection();
        let challenge = registry
            .begin_challenge(
                &reconnect,
                identity.device_id.clone(),
                identity.key_fingerprint.clone(),
            )
            .unwrap();
        let signature = sign_challenge(&keys, &challenge, identity.identity_epoch);
        assert_eq!(
            verify_node_auth(&registry, &reconnect, &signature, identity.identity_epoch),
            Err(NodeErrorCode::IdentityRejected)
        );
    }

    #[test]
    fn failed_handshakes_do_not_grow_challenge_table() {
        let registry = NodeRegistry::new(2);
        let a = registry.mint_connection();
        let b = registry.mint_connection();
        let c = registry.mint_connection();
        assert!(
            registry
                .begin_challenge(&a, "dev-a".into(), "fp-a".into())
                .is_ok()
        );
        assert!(
            registry
                .begin_challenge(&b, "dev-b".into(), "fp-b".into())
                .is_ok()
        );
        assert_eq!(
            registry
                .begin_challenge(&c, "dev-c".into(), "fp-c".into())
                .unwrap_err(),
            crate::device_identity::IdentityError::Capacity
        );
        assert_eq!(registry.pending_challenge_count(), 2);
        registry.detach_socket(&a.connection_id);
        registry.detach_socket(&b.connection_id);
        assert_eq!(registry.pending_challenge_count(), 0);
    }

    #[test]
    fn preauth_sockets_count_against_max_nodes() {
        let registry = NodeRegistry::new(1);
        let (conn, _rx) = registry.try_reserve().expect("first pre-auth slot");
        assert!(
            registry.try_reserve().is_none(),
            "second pre-auth socket must hit the quota"
        );
        registry.detach_socket(&conn.connection_id);
        assert!(registry.try_reserve().is_some());
    }

    #[test]
    fn advertise_frame_admits_subset_and_refuses_widen() {
        let registry = NodeRegistry::new(8);
        let (_keys, identity) = enroll_test_device(&registry, vec!["system.notify".into()]);
        let (conn, _rx) = registry.try_reserve().expect("reserve");
        registry.bind_identity(
            &conn.connection_id,
            identity.device_id.clone(),
            identity.key_fingerprint.clone(),
        );
        let admitted = process_post_handshake_text(
            &registry,
            &conn,
            r#"{"type":"advertise","caps":["system.notify"],"cap_revision":1}"#,
        )
        .expect("advertise frame");
        match admitted {
            (GatewayToNode::Admitted { caps, cap_revision }, false) => {
                assert_eq!(caps, ["system.notify"]);
                assert_eq!(cap_revision, 1);
            }
            other => panic!("expected admitted, got {other:?}"),
        }
        let widen = process_post_handshake_text(
            &registry,
            &conn,
            r#"{"type":"advertise","caps":["system.notify","camera.snap"],"cap_revision":2}"#,
        )
        .expect("widen frame");
        match widen {
            (
                GatewayToNode::Error {
                    code: NodeErrorCode::CapabilityWiden,
                    ..
                },
                false,
            ) => {}
            other => panic!("expected capability_widen without teardown, got {other:?}"),
        }
        let repeated = process_post_handshake_text(
            &registry,
            &conn,
            r#"{"type":"advertise","caps":["camera.snap"],"cap_revision":3}"#,
        )
        .expect("repeated widen");
        match repeated {
            (
                GatewayToNode::Error {
                    code: NodeErrorCode::CapabilityWiden,
                    ..
                },
                true,
            ) => {}
            other => panic!("repeated widen must tear down, got {other:?}"),
        }
        assert_eq!(
            registry
                .identities()
                .active_identity(&identity.device_id, &identity.key_fingerprint)
                .unwrap()
                .capability_ceiling,
            ["system.notify"]
        );
    }

    async fn spawn_nodes_chat_server(state: crate::AppState) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/ws/nodes", axum::routing::get(handle_ws_nodes))
            .route("/ws/chat", axum::routing::get(crate::ws::handle_ws_chat))
            .with_state(state);
        zeroclaw_spawn::spawn!(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        addr
    }

    async fn http_upgrade(
        addr: SocketAddr,
        path: &str,
        protocol: Option<&str>,
        auth: Option<&str>,
    ) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = tokio::net::TcpStream::connect(addr).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut stream = stream.expect("test server accepted connections");
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
        );
        if let Some(proto) = protocol {
            req.push_str(&format!("Sec-WebSocket-Protocol: {proto}\r\n"));
        }
        if let Some(token) = auth {
            req.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
        let status = raw
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, raw.to_ascii_lowercase(), raw)
    }

    fn nodes_integration_state() -> crate::AppState {
        let mut config = zeroclaw_config::schema::Config::default();
        config.nodes.enabled = true;
        config.nodes.auth_token = Some("secret".into());
        config.risk_profiles.insert(
            "test-profile".to_string(),
            zeroclaw_config::schema::RiskProfileConfig::default(),
        );
        config.providers.models.openrouter.insert(
            "default".to_string(),
            zeroclaw_config::schema::OpenRouterModelProviderConfig::default(),
        );
        config.agents.insert(
            "test-agent".to_string(),
            zeroclaw_config::schema::AliasedAgentConfig {
                model_provider: "openrouter.default".into(),
                risk_profile: "test-profile".into(),
                ..Default::default()
            },
        );
        crate::api::test_state(config)
    }

    #[tokio::test]
    async fn nodes_v2_upgrade_missing_subprotocol_returns_400() {
        let addr = spawn_nodes_chat_server(nodes_integration_state()).await;
        let (status, _lower, body) = http_upgrade(addr, "/ws/nodes", None, Some("secret")).await;
        assert_eq!(status, 400, "body={body}");
        assert!(
            body.contains("protocol_unsupported"),
            "typed reject body={body}"
        );
    }

    #[tokio::test]
    async fn nodes_v2_upgrade_echoes_subprotocol_on_101() {
        let addr = spawn_nodes_chat_server(nodes_integration_state()).await;
        let (status, lower, body) =
            http_upgrade(addr, "/ws/nodes", Some(WS_NODES_V2), Some("secret")).await;
        assert_eq!(status, 101, "body={body}");
        assert!(
            lower.contains("sec-websocket-protocol: zeroclaw.nodes.v2"),
            "response must echo v2 subprotocol: {body}"
        );
    }

    #[tokio::test]
    async fn chat_ws_upgrade_succeeds_without_subprotocol() {
        let addr = spawn_nodes_chat_server(nodes_integration_state()).await;
        let (status, _lower, body) =
            http_upgrade(addr, "/ws/chat?agent=test-agent", None, None).await;
        assert_eq!(status, 101, "chat must upgrade without subprotocol: {body}");
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
