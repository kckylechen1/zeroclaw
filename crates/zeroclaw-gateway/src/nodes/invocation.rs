//! Bounded read-only invocation on the authenticated v2 socket.
use super::*;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use zeroclaw_api::attribution::{Attributable, Role, ToolKind};
use zeroclaw_api::node_tools::NodeTools;
use zeroclaw_api::tool::{Tool, ToolResult};

pub const SAFE_CAPS: [&str; 2] = ["system.info", "test.echo"];
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PENDING: usize = 16;

pub(super) struct Pending {
    revision: u64,
    cap: String,
    sent: bool,
    tx: oneshot::Sender<ToolResult>,
}

pub(super) struct InvocationState {
    pub caps: Vec<String>,
    pub revision: u64,
    pub advertised: bool,
    pub tx: Option<mpsc::Sender<GatewayToNode>>,
    pending: HashMap<String, Pending>,
}

impl InvocationState {
    pub fn new() -> Self {
        Self {
            caps: Vec::new(),
            revision: 0,
            advertised: false,
            tx: None,
            pending: HashMap::new(),
        }
    }
    pub fn drain(&mut self, reason: &str) {
        for (_, pending) in self.pending.drain() {
            let _ = pending.tx.send(ToolResult::err(reason));
        }
    }
}

pub fn validate_args(cap: &str, args: &serde_json::Value) -> bool {
    let Some(object) = args.as_object() else {
        return false;
    };
    match cap {
        "system.info" => object.is_empty(),
        "test.echo" => {
            object.len() == 1
                && object
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| s.len() <= 4096)
        }
        _ => false,
    }
}

pub fn args_digest(args: &serde_json::Value) -> String {
    hex::encode(Sha256::digest(args.to_string().as_bytes()))
}

impl NodeRegistry {
    pub(super) fn attach_invocations(&self, connection: &str) -> mpsc::Receiver<GatewayToNode> {
        let (tx, rx) = mpsc::channel(MAX_PENDING);
        if let Some(socket) = self.live.write().get_mut(connection) {
            socket.invocation.tx = Some(tx);
        }
        rx
    }

    pub(super) fn set_caps(&self, connection: &str, caps: Vec<String>, revision: u64) -> bool {
        let mut live = self.live.write();
        let Some(socket) = live.get_mut(connection) else {
            return false;
        };
        let state = &mut socket.invocation;
        if state.advertised && revision <= state.revision {
            return false;
        }
        if state.advertised {
            state.drain("capability_changed");
        }
        state.caps = caps;
        state.revision = revision;
        state.advertised = true;
        true
    }

    /// Close admission before draining, so shutdown cannot acquire new pending work.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let mut live = self.live.write();
        for socket in live.values_mut() {
            socket.invocation.drain("shutdown_unknown_outcome");
            let _ = socket.close_tx.send(true);
        }
        live.clear();
    }

    pub fn pending_invocations(&self) -> usize {
        self.live
            .read()
            .values()
            .map(|s| s.invocation.pending.len())
            .sum()
    }

    /// Revalidate at the writer, after dequeue. Mark possibly-sent before any await.
    pub(super) fn prepare_send(&self, connection: &str, frame: &GatewayToNode) -> bool {
        let GatewayToNode::Invoke {
            call_id, deadline, ..
        } = frame
        else {
            return false;
        };
        let Some((device, fingerprint)) = self.bound_identity(connection) else {
            return false;
        };
        let Some(store) = self.identities() else {
            return false;
        };
        store
            .with_active(&device, &fingerprint, || {
                let mut live = self.live.write();
                let Some(socket) = live.get_mut(connection) else {
                    return false;
                };
                let state = &mut socket.invocation;
                let valid = !self.stopped.load(Ordering::SeqCst)
                    && chrono::DateTime::parse_from_rfc3339(deadline).is_ok_and(|d| d > Utc::now())
                    && state.pending.get(call_id).is_some_and(|p| {
                        p.revision == state.revision && state.caps.contains(&p.cap)
                    });
                if !valid {
                    if let Some(pending) = state.pending.remove(call_id) {
                        let _ = pending.tx.send(ToolResult::err("before_send_refused"));
                    }
                    return false;
                }
                if let Some(pending) = state.pending.get_mut(call_id) {
                    pending.sent = true;
                }
                true
            })
            .unwrap_or(false)
    }

    pub(super) fn complete(&self, connection: &str, call_id: &str, result: ToolResult) {
        let Some((device, fingerprint)) = self.bound_identity(connection) else {
            return;
        };
        let Some(store) = self.identities() else {
            return;
        };
        store.with_active(&device, &fingerprint, || {
            let mut live = self.live.write();
            let Some(socket) = live.get_mut(connection) else {
                return;
            };
            if let Some(pending) = socket.invocation.pending.remove(call_id) {
                let _ = pending.tx.send(result);
            }
        });
    }

    async fn invoke(
        &self,
        device: &str,
        connection: &str,
        revision: u64,
        cap: &str,
        args: serde_json::Value,
    ) -> ToolResult {
        if !validate_args(cap, &args) {
            return ToolResult::err("invalid_or_unsupported_capability");
        }
        let Some((bound_device, fingerprint)) = self.bound_identity(connection) else {
            return ToolResult::err("stale_connection");
        };
        if bound_device != device {
            return ToolResult::err("identity_rejected");
        }
        let Some(store) = self.identities() else {
            return ToolResult::err("identity_unavailable");
        };
        let call_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let deadline = Utc::now() + chrono::Duration::seconds(2);
        let admitted = store
            .with_active(device, &fingerprint, || {
                let mut live = self.live.write();
                let socket = live.get_mut(connection).ok_or("stale_connection")?;
                let state = &mut socket.invocation;
                if self.stopped.load(Ordering::SeqCst) {
                    return Err("shutdown");
                }
                if !state.advertised
                    || state.revision != revision
                    || !state.caps.iter().any(|c| c == cap)
                {
                    return Err("capability_changed");
                }
                if state.pending.len() >= MAX_PENDING {
                    return Err("busy");
                }
                let sender = state.tx.as_ref().ok_or("unavailable")?;
                let frame = GatewayToNode::Invoke {
                    call_id: call_id.clone(),
                    connection_id: connection.into(),
                    cap: cap.into(),
                    cap_revision: revision,
                    args_digest: args_digest(&args),
                    args,
                    deadline: deadline.to_rfc3339(),
                    grant_proof: None,
                };
                // Linearization order: pending first, then queue, under the same lock.
                state.pending.insert(
                    call_id.clone(),
                    Pending {
                        revision,
                        cap: cap.into(),
                        sent: false,
                        tx,
                    },
                );
                if sender.try_send(frame).is_err() {
                    state.pending.remove(&call_id);
                    return Err("before_send_busy");
                }
                Ok(())
            })
            .unwrap_or(Err("identity_rejected"));
        if let Err(reason) = admitted {
            return ToolResult::err(reason);
        }
        // Drop cancellation also removes the pending entry. Wire work is never replayed.
        let _cleanup = PendingGuard {
            registry: self.clone(),
            connection: connection.into(),
            call_id: call_id.clone(),
        };
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => ToolResult::err("disconnected_unknown_outcome"),
            Err(_) => {
                let mut live = self.live.write();
                let sent = live
                    .get_mut(connection)
                    .and_then(|s| s.invocation.pending.remove(&call_id))
                    .is_some_and(|p| p.sent);
                ToolResult::err(if sent {
                    "timeout_unknown_outcome"
                } else {
                    "timeout_before_send"
                })
            }
        }
    }
}

struct PendingGuard {
    registry: NodeRegistry,
    connection: String,
    call_id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(socket) = self.registry.live.write().get_mut(&self.connection) {
            socket.invocation.pending.remove(&self.call_id);
        }
    }
}

struct NodeTool {
    registry: NodeRegistry,
    name: String,
    device: String,
    connection: String,
    revision: u64,
    cap: String,
}
impl Attributable for NodeTool {
    fn role(&self) -> Role {
        Role::Tool(ToolKind::Plugin)
    }
    fn alias(&self) -> &str {
        &self.name
    }
}
#[async_trait::async_trait]
impl Tool for NodeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Read-only info or echo from an explicitly authorized local node."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        if self.cap == "test.echo" {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string","maxLength":4096}},"required":["text"],"additionalProperties":false})
        } else {
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
        }
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(self
            .registry
            .invoke(
                &self.device,
                &self.connection,
                self.revision,
                &self.cap,
                args,
            )
            .await)
    }
}
impl NodeTools for NodeRegistry {
    fn tools(&self, allowed: &[String]) -> Vec<Box<dyn Tool>> {
        if self.stopped.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        for socket in self.live.read().values() {
            let Some(device) = &socket.device_id else {
                continue;
            };
            for cap in &socket.invocation.caps {
                let name = format!("node:{device}:{cap}");
                if SAFE_CAPS.contains(&cap.as_str()) && allowed.contains(&name) {
                    tools.push(Box::new(NodeTool {
                        registry: self.clone(),
                        name,
                        device: device.clone(),
                        connection: socket.connection_id.clone(),
                        revision: socket.invocation.revision,
                        cap: cap.clone(),
                    }));
                }
            }
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued_registry() -> (NodeRegistry, String, String, mpsc::Receiver<GatewayToNode>) {
        let registry = NodeRegistry::new(2);
        let key = crate::device_identity::DeviceKeyPair::generate().unwrap();
        let store = registry.identities().unwrap();
        let code = store.issue_pairing_code(vec!["test.echo".into()]).unwrap();
        let identity = store.enroll(&code, key.public_key_hex()).unwrap();
        let (connection, _close) = registry.try_reserve().unwrap();
        registry
            .bind_identity(
                &connection.connection_id,
                identity.device_id.clone(),
                identity.key_fingerprint,
            )
            .unwrap();
        assert!(registry.set_caps(&connection.connection_id, vec!["test.echo".into()], 1));
        let receiver = registry.attach_invocations(&connection.connection_id);
        (
            registry,
            identity.device_id,
            connection.connection_id,
            receiver,
        )
    }

    #[tokio::test]
    async fn unsent_timeout_and_cancel_never_reach_writer() {
        let (registry, device, connection, mut wire) = queued_registry();
        let result = registry
            .invoke(
                &device,
                &connection,
                1,
                "test.echo",
                serde_json::json!({"text":""}),
            )
            .await;
        assert_eq!(result.error.as_deref(), Some("timeout_before_send"));
        assert!(!registry.prepare_send(&connection, &wire.recv().await.unwrap()));
        assert_eq!(registry.pending_invocations(), 0);
        let r = registry.clone();
        let d = device.clone();
        let c = connection.clone();
        let task = tokio::spawn(async move {
            r.invoke(&d, &c, 1, "test.echo", serde_json::json!({"text":""}))
                .await
        });
        let frame = wire.recv().await.unwrap();
        assert_eq!(registry.pending_invocations(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(registry.pending_invocations(), 0);
        assert!(!registry.prepare_send(&connection, &frame));
    }

    #[tokio::test]
    async fn reduction_invalidates_queued_frame_before_send() {
        let (registry, device, connection, mut wire) = queued_registry();
        let r = registry.clone();
        let c = connection.clone();
        let task = tokio::spawn(async move {
            r.invoke(&device, &c, 1, "test.echo", serde_json::json!({"text":""}))
                .await
        });
        let frame = wire.recv().await.unwrap();
        assert_eq!(registry.pending_invocations(), 1);
        assert!(registry.set_caps(&connection, vec![], 2));
        assert!(!registry.prepare_send(&connection, &frame));
        assert_eq!(
            task.await.unwrap().error.as_deref(),
            Some("capability_changed")
        );
        assert_eq!(registry.pending_invocations(), 0);
        assert!(!registry.set_caps(&connection, vec!["test.echo".into()], 1));
    }
}
