//! Thin client for the gateway's `/ws/chat` WebSocket.
//!
//! The gateway owns the conversation (agent, history, approvals). A client
//! attaches to a session, sends messages, answers approvals and streams the
//! frames every attached client receives. See `docs/book/src/gateway/api.md`
//! for the protocol.
//!
//! This crate depends only on transport and serialization, so a CLI or a
//! chat bridge can use it without pulling in the runtime.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The chat sub-protocol the gateway speaks.
pub const PROTOCOL: &str = "zeroclaw.v1";

/// Where and as whom to attach.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Gateway base URL, `ws://host:port` or `wss://host:port`, optionally
    /// with a path prefix.
    pub gateway: String,
    /// Agent alias the session runs as.
    pub agent: String,
    /// Session to attach to; the gateway creates one when absent.
    pub session_id: Option<String>,
    /// Paired bearer token, when the gateway requires pairing.
    pub token: Option<String>,
}

impl ConnectOptions {
    /// The `/ws/chat` URL for these options. The token travels in the
    /// `Authorization` header, never in the URL.
    pub fn chat_url(&self) -> String {
        let mut url = format!(
            "{}/ws/chat?agent={}",
            self.gateway.trim_end_matches('/'),
            encode(&self.agent)
        );
        if let Some(session) = &self.session_id {
            url.push_str("&session_id=");
            url.push_str(&encode(session));
        }
        url
    }
}

/// Percent-encode a query value (RFC 3986 unreserved characters pass).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The gateway answered but refused the upgrade (bad token, unknown agent,
/// ...). Returned inside the `anyhow::Error` from [`Client::connect`];
/// downcast to tell it from an unreachable gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub status: u16,
    pub reason: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gateway refused the connection ({}): {}",
            self.status, self.reason
        )
    }
}

impl std::error::Error for Rejected {}

/// What the gateway said when the socket attached.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionStart {
    pub session_id: String,
    #[serde(default)]
    pub resumed: bool,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub name: Option<String>,
}

/// An operator decision on an `approval_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    /// Approve and stop asking for this tool.
    Always,
    Deny,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Always => "always",
            Self::Deny => "deny",
        }
    }
}

/// A frame from the gateway. Frames this client does not model arrive as
/// [`Frame::Other`], so a newer gateway does not break an older client.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// A message was accepted (or recognised as a duplicate).
    Ack {
        id: String,
        status: String,
        #[serde(default)]
        turn: Option<String>,
        #[serde(default)]
        durable: Option<bool>,
        /// Last recorded state, on duplicates.
        #[serde(default)]
        state: Option<String>,
    },
    Chunk {
        content: String,
    },
    Thinking {
        content: String,
    },
    ToolCall {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    ToolResult {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        output: serde_json::Value,
    },
    ApprovalRequest {
        request_id: String,
        tool: String,
        #[serde(default)]
        arguments_summary: String,
        #[serde(default)]
        timeout_secs: u64,
    },
    /// The turn finished.
    Done {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        full_response: String,
    },
    /// The turn was cancelled.
    Aborted {
        #[serde(default)]
        id: Option<String>,
    },
    Error {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default)]
        code: Option<String>,
    },
    #[serde(skip)]
    Other(serde_json::Value),
}

impl Frame {
    /// Parse one text frame.
    pub fn parse(text: &str) -> Result<Self> {
        let value: serde_json::Value =
            serde_json::from_str(text).context("gateway sent a frame that is not JSON")?;
        Ok(Frame::deserialize(&value).unwrap_or(Frame::Other(value)))
    }

    /// Whether this frame ends a turn.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Frame::Done { .. } | Frame::Aborted { .. } | Frame::Error { .. }
        )
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// An attached chat socket.
pub struct Client {
    socket: Socket,
    session: SessionStart,
}

impl Client {
    /// Connect, read the gateway's `session_start`, and complete the
    /// handshake so the session is ready for messages.
    pub async fn connect(options: &ConnectOptions) -> Result<Self> {
        let url = options.chat_url();
        let mut request = url
            .as_str()
            .into_client_request()
            .with_context(|| format!("invalid gateway URL: {url}"))?;
        let headers = request.headers_mut();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(PROTOCOL),
        );
        if let Some(token) = &options.token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("gateway token is not a valid header value")?;
            headers.insert(header::AUTHORIZATION, value);
        }
        let (mut socket, _) = match tokio_tungstenite::connect_async(request).await {
            Ok(connected) => connected,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let reason = response
                    .body()
                    .as_deref()
                    .map(|body| String::from_utf8_lossy(body).trim().to_string())
                    .filter(|body| !body.is_empty())
                    .unwrap_or_else(|| response.status().to_string());
                return Err(Rejected {
                    status: response.status().as_u16(),
                    reason,
                }
                .into());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("could not connect to {}", options.gateway));
            }
        };

        let session = match next_value(&mut socket).await? {
            Some(value) if value["type"] == "session_start" => {
                serde_json::from_value::<SessionStart>(value)
                    .context("malformed session_start frame")?
            }
            Some(value) => bail!("gateway did not start a session: {value}"),
            None => bail!("gateway closed the connection before starting a session"),
        };

        // The gateway builds the agent after the first client frame; a
        // `connect` frame lets it do so before the first message.
        send_json(&mut socket, &serde_json::json!({ "type": "connect" })).await?;
        loop {
            match next_value(&mut socket).await? {
                Some(value) if value["type"] == "connected" => break,
                Some(value) if value["type"] == "error" => {
                    bail!(
                        "gateway refused the session: {}",
                        value["message"].as_str().unwrap_or("unknown error")
                    )
                }
                // Restore notices and cron results may precede `connected`.
                Some(_) => continue,
                None => bail!("gateway closed the connection during the handshake"),
            }
        }
        Ok(Self { socket, session })
    }

    pub fn session(&self) -> &SessionStart {
        &self.session
    }

    /// Send a message with a fresh request id and return the id. The
    /// matching [`Frame::Ack`] arrives through [`Client::next_frame`].
    pub async fn send_message(&mut self, content: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        send_json(
            &mut self.socket,
            &serde_json::json!({ "type": "message", "content": content, "id": id }),
        )
        .await?;
        Ok(id)
    }

    /// Ask the gateway to stop the session's running turn.
    pub async fn cancel(&mut self) -> Result<()> {
        send_json(&mut self.socket, &serde_json::json!({ "type": "cancel" })).await
    }

    /// Answer an `approval_request`.
    pub async fn answer_approval(&mut self, request_id: &str, decision: Decision) -> Result<()> {
        send_json(
            &mut self.socket,
            &serde_json::json!({
                "type": "approval_response",
                "request_id": request_id,
                "decision": decision.as_str(),
            }),
        )
        .await
    }

    /// The next frame, or `None` once the gateway closes the socket.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            match self.socket.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Text(text))) => return Frame::parse(&text).map(Some),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e).context("gateway connection failed"),
            }
        }
    }

    /// Close the socket. The session and any running turn stay on the
    /// gateway.
    pub async fn close(mut self) -> Result<()> {
        self.socket.close(None).await.context("closing the socket")
    }
}

async fn send_json(socket: &mut Socket, value: &serde_json::Value) -> Result<()> {
    socket
        .send(Message::Text(Utf8Bytes::from(value.to_string())))
        .await
        .context("sending to the gateway")
}

async fn next_value(socket: &mut Socket) -> Result<Option<serde_json::Value>> {
    loop {
        match socket.next().await {
            None | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text)
                    .map(Some)
                    .context("gateway sent a frame that is not JSON");
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e).context("gateway connection failed"),
        }
    }
}

#[cfg(test)]
mod tests;
