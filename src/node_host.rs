//! Foreground loopback-only v2 host. Private key stays in this process.
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::net::SocketAddr;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use zeroclaw_api::node::{GatewayToNode, NodeToGateway, WS_NODES_V2};
use zeroclaw_gateway::device_identity::{DeviceKeyPair, auth_message};
use zeroclaw_gateway::nodes::invocation::{SAFE_CAPS, args_digest, validate_args};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(socket: &mut Socket, message: Message) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), socket.send(message)).await??;
    Ok(())
}
async fn close(socket: &mut Socket) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await??;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    address: SocketAddr,
    device_id: String,
    identity_epoch: u64,
    token: String,
}

fn emit(value: Value) {
    println!("{value}");
}

/// Prints a public enrollment key, then reads one operator-issued identity from
/// stdin. Pair it with the existing gateway identity API before supplying it.
/// No token, private key, hostname, account, path or raw result is logged.
pub async fn run() -> Result<()> {
    let terminate = async {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::pin!(terminate);
    let key = DeviceKeyPair::generate().map_err(|e| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure),
            "node host key generation failed"
        );
        anyhow::Error::msg(e.to_string())
    })?;
    emit(json!({"event":"public_key", "public_key":key.public_key_hex()}));
    // Tokio stdin uses an uncancellable blocking read that can keep its runtime
    // alive after socket shutdown. A detached, bounded input thread has no such
    // ownership over process exit.
    let (input_tx, mut input) = tokio::sync::mpsc::channel(4);
    std::thread::spawn(move || {
        let mut reader = std::io::stdin().lock();
        loop {
            let mut line = String::new();
            let read = (&mut reader).take(16_385).read_line(&mut line);
            let value = match read {
                Ok(0) => break,
                Ok(n) if n <= 16_384 => Ok(line.trim_end_matches(['\r', '\n']).to_owned()),
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stdin line too large",
                )),
                Err(error) => Err(error),
            };
            let failed = value.is_err();
            if input_tx.blocking_send(value).is_err() || failed {
                break;
            }
        }
    });
    let line = input
        .recv()
        .await
        .transpose()?
        .context("missing enrolled identity on stdin")?;
    let session: Session = serde_json::from_str(&line)?;
    if !session.address.ip().is_loopback() || session.device_id.len() > 256 {
        bail!("loopback address and bounded device id required")
    }
    #[cfg(test)]
    let mut hold_results = false;
    // Local reductions survive reconnect; a fresh connection grants nothing new.
    let mut caps: Vec<String> = SAFE_CAPS.iter().map(|s| (*s).into()).collect();
    loop {
        let mut request = format!("ws://{}/ws/nodes", session.address).into_client_request()?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", WS_NODES_V2.parse()?);
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", session.token).parse()?,
        );
        let (mut socket, response) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(
                request,
                Some(
                    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                        .max_message_size(Some(64 * 1024))
                        .max_frame_size(Some(64 * 1024)),
                ),
                false,
            ),
        )
        .await??;
        if response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|v| v.to_str().ok())
            != Some(WS_NODES_V2)
        {
            bail!("v2 subprotocol required")
        }
        let hello = NodeToGateway::Hello {
            protocol_versions: vec!["2.0".into()],
            device_id: Some(session.device_id.clone()),
            key_fingerprint: Some(key.fingerprint().into()),
        };
        send(
            &mut socket,
            Message::Text(serde_json::to_string(&hello)?.into()),
        )
        .await?;
        let mut connection = String::new();
        let mut revision = 1;
        let mut seen: HashMap<String, (String, NodeToGateway)> = HashMap::new();
        let mut authenticated = false;
        let handshake_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                _ = &mut terminate => { close(&mut socket).await?; return Ok(()) }
                _ = tokio::signal::ctrl_c() => { close(&mut socket).await?; return Ok(()) }
                _ = tokio::time::sleep_until(handshake_deadline), if !authenticated => bail!("handshake timeout"),
                line = input.recv() => {
                    let Some(line) = line.transpose()? else { close(&mut socket).await?; return Ok(()) };
                    match line.as_str() {
                        "stop" => { close(&mut socket).await?; return Ok(()) }
                        "reconnect" => { close(&mut socket).await?; break }
                        "info-only" if authenticated => {
                            caps = vec!["system.info".into()]; revision += 1;
                            send(&mut socket, Message::Text(serde_json::to_string(&NodeToGateway::Advertise { caps: caps.clone(), cap_revision: revision })?.into())).await?;
                        }
                        #[cfg(test)]
                        "release" => {
                            hold_results = false;
                            for (_, result) in seen.values() {
                                send(&mut socket, Message::Text(serde_json::to_string(result)?.into())).await?;
                            }
                            emit(json!({"event":"released"}));
                        }
                        #[cfg(test)]
                        "hold" => { hold_results = true; emit(json!({"event":"holding"})); }
                        _ => bail!("unsupported foreground command"),
                    }
                }
                message = socket.next() => {
                    let Some(message) = message else { return Ok(()) };
                    let text = match message? {
                        Message::Text(text) => text,
                        Message::Ping(bytes) => { send(&mut socket, Message::Pong(bytes)).await?; continue }
                        Message::Close(_) => return Ok(()),
                        _ => continue,
                    };
                    let frame: GatewayToNode = serde_json::from_str(&text)?;
                    match frame {
                        GatewayToNode::Challenge { nonce, .. } if !authenticated => {
                            let auth = NodeToGateway::Auth { signature: key.sign(&auth_message(&nonce, &session.device_id, key.fingerprint(), session.identity_epoch)), identity_epoch: session.identity_epoch };
                            send(&mut socket, Message::Text(serde_json::to_string(&auth)?.into())).await?;
                        }
                        GatewayToNode::HelloAck { connection_id, protocol_version, .. } if !authenticated && protocol_version == "2.0" => {
                            connection = connection_id;
                            send(&mut socket, Message::Text(serde_json::to_string(&NodeToGateway::Advertise { caps: caps.clone(), cap_revision: revision })?.into())).await?;
                        }
                        GatewayToNode::Admitted { caps: admitted, cap_revision } if cap_revision == revision && admitted == caps => {
                            authenticated = true;
                            emit(json!({"event":"ready", "connection":connection, "revision":revision}));
                        }
                        GatewayToNode::Invoke { call_id, connection_id, cap, cap_revision, args, args_digest: digest, deadline, grant_proof } => {
                            let identity = format!("{connection_id}:{cap_revision}:{cap}:{digest}:{deadline}");
                            let refusal = if !authenticated || connection_id != connection { Some("stale_connection") }
                                else if cap_revision != revision || !caps.contains(&cap) { Some("capability_changed") }
                                else if grant_proof.is_some() || !validate_args(&cap, &args) || args_digest(&args) != digest { Some("unsupported_or_invalid") }
                                else if !chrono::DateTime::parse_from_rfc3339(&deadline).is_ok_and(|d| d > chrono::Utc::now()) { Some("deadline_expired") }
                                else { None };
                            let result = if let Some(reason) = refusal {
                                NodeToGateway::Result { call_id: call_id.clone(), connection_id: connection.clone(), success: false, output: String::new(), error: Some(reason.into()) }
                            } else if let Some((previous, result)) = seen.get(&call_id) {
                                if previous != &identity { bail!("conflicting duplicate call") }
                                result.clone()
                            } else {
                                if seen.len() >= 1024 { bail!("session call limit reached; reconnect explicitly") }
                                let output = if cap == "test.echo" { args["text"].as_str().context("validated echo")?.to_owned() }
                                    else { json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH}).to_string() };
                                let result = NodeToGateway::Result { call_id: call_id.clone(), connection_id: connection.clone(), success: true, output, error: None };
                                // Reserve before returning any result; never evict/re-execute IDs.
                                seen.insert(call_id.clone(), (identity, result.clone()));
                                emit(json!({"event":"executed", "call_id":call_id, "count":seen.len()}));
                                result
                            };
                            #[cfg(test)]
                            if hold_results { continue }
                            let encoded = serde_json::to_string(&result)?;
                            send(&mut socket, Message::Text(encoded.clone().into())).await?;
                            // The child-process proof deliberately duplicates a real result.
                            #[cfg(test)]
                            send(&mut socket, Message::Text(encoded.into())).await?;
                        }
                        GatewayToNode::Error { code, .. } => bail!("gateway refusal: {}", code.as_str()),
                        _ => bail!("unexpected v2 frame"),
                    }
                }
            }
        }
    }
}
