//! `zeroclaw chat`: a thin client of the gateway's chat socket.
//!
//! The gateway owns the conversation; this command only attaches to a
//! session, forwards what the user types and renders what every attached
//! client receives. Closing it leaves a running turn going.

use std::io::Write;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use zeroclaw_gateway_client::{Client, ConnectOptions, Decision, Frame, Rejected};

use crate::commands::self_test::{resolve_gateway_bearer_token, resolve_probe_host};
use crate::config::Config;
use crate::gateway_helpers::ta;

/// This machine's gateway as a WebSocket base URL.
fn local_gateway_url(config: &Config) -> String {
    let (host, _) = resolve_probe_host(&config.gateway.host);
    let prefix = config.gateway.path_prefix.as_deref().unwrap_or("");
    format!("ws://{host}:{}{prefix}", config.gateway.port)
}

/// What a typed line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Input<'a> {
    Quit,
    Cancel,
    Nothing,
    Message(&'a str),
}

fn parse_input(line: &str) -> Input<'_> {
    match line.trim() {
        "/quit" | "/exit" => Input::Quit,
        "/cancel" => Input::Cancel,
        "" => Input::Nothing,
        text => Input::Message(text),
    }
}

/// An answer to an approval prompt. Anything but yes or always denies.
fn parse_decision(line: &str) -> Decision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Decision::Approve,
        "a" | "always" => Decision::Always,
        _ => Decision::Deny,
    }
}

pub async fn run(
    config: &Config,
    agent: String,
    session: String,
    gateway: Option<String>,
    message: Option<String>,
) -> Result<()> {
    let gateway = gateway.unwrap_or_else(|| local_gateway_url(config));
    let options = ConnectOptions {
        gateway: gateway.clone(),
        agent: agent.clone(),
        session_id: Some(session),
        token: resolve_gateway_bearer_token(config),
    };
    let mut client = match Client::connect(&options).await {
        Ok(client) => client,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "gateway": &gateway,
                        "error": format!("{e:#}"),
                    })),
                "chat could not attach to the gateway"
            );
            // A refusal (unknown agent, bad token) needs a different fix
            // than an unreachable gateway.
            let key = if e.downcast_ref::<Rejected>().is_some() {
                "cli-chat-connect-refused"
            } else {
                "cli-chat-connect-failed"
            };
            anyhow::bail!(ta(
                key,
                &[("gateway", &gateway), ("error", &format!("{e:#}"))],
                "Could not attach to the gateway",
            ));
        }
    };

    if let Some(message) = message {
        client.send_message(&message).await?;
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Some(frame) = client.next_frame().await? {
            if let Frame::ApprovalRequest { request_id, .. } = &frame {
                render(&frame);
                let line = stdin.next_line().await?.unwrap_or_default();
                client
                    .answer_approval(request_id, parse_decision(&line))
                    .await?;
                continue;
            }
            render(&frame);
            if frame.is_terminal() {
                break;
            }
        }
        return client.close().await;
    }

    let history = client.session().message_count.to_string();
    let session_id = client.session().session_id.clone();
    println!(
        "{}",
        ta(
            "cli-chat-attached",
            &[
                ("session", &session_id),
                ("agent", &agent),
                ("history", &history)
            ],
            "Attached",
        )
    );

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut pending_approval: Option<String> = None;
    loop {
        tokio::select! {
            line = stdin.next_line() => {
                let Some(line) = line? else { break };
                if let Some(request_id) = pending_approval.take() {
                    client.answer_approval(&request_id, parse_decision(&line)).await?;
                    continue;
                }
                match parse_input(&line) {
                    Input::Quit => break,
                    Input::Cancel => client.cancel().await?,
                    Input::Nothing => {}
                    Input::Message(text) => {
                        client.send_message(text).await?;
                    }
                }
            }
            frame = client.next_frame() => {
                let Some(frame) = frame? else {
                    println!("{}", ta("cli-chat-closed", &[], "Connection closed"));
                    return Ok(());
                };
                if let Frame::ApprovalRequest { request_id, .. } = &frame {
                    pending_approval = Some(request_id.clone());
                }
                render(&frame);
            }
            _ = tokio::signal::ctrl_c() => {
                client.cancel().await?;
            }
        }
    }
    client.close().await
}

/// A one-line context and cost summary for a finished turn, when the
/// gateway reported the numbers.
fn usage_line(
    last_input_tokens: Option<u64>,
    max_context_tokens: Option<u64>,
    cost_usd: Option<f64>,
) -> Option<String> {
    let used = last_input_tokens?;
    let context = match max_context_tokens.filter(|max| *max > 0) {
        Some(max) => format!("{}k/{}k ({}%)", used / 1000, max / 1000, used * 100 / max),
        None => format!("{}k", used / 1000),
    };
    let cost = cost_usd.map(|usd| format!("${usd:.4}")).unwrap_or_default();
    Some(ta(
        "cli-chat-usage",
        &[("context", &context), ("cost", &cost)],
        "usage",
    ))
}

/// Print one frame for the terminal.
fn render(frame: &Frame) {
    match frame {
        Frame::Chunk { content } => {
            print!("{content}");
            let _ = std::io::stdout().flush();
        }
        Frame::ToolCall { name, .. } => {
            println!("\n{}", ta("cli-chat-tool-call", &[("tool", name)], "tool"));
        }
        Frame::ApprovalRequest {
            tool,
            arguments_summary,
            ..
        } => {
            print!(
                "\n{} ",
                ta(
                    "cli-chat-approval-prompt",
                    &[("tool", tool), ("summary", arguments_summary)],
                    "Allow?",
                )
            );
            let _ = std::io::stdout().flush();
        }
        Frame::Done {
            last_input_tokens,
            max_context_tokens,
            cost_usd,
            ..
        } => {
            println!();
            if let Some(line) = usage_line(*last_input_tokens, *max_context_tokens, *cost_usd) {
                println!("{line}");
            }
        }
        Frame::Aborted { .. } => println!("\n{}", ta("cli-chat-aborted", &[], "cancelled")),
        Frame::Error { message, .. } => {
            eprintln!(
                "\n{}",
                ta("cli-chat-error", &[("message", message)], "error")
            );
        }
        Frame::Ack {
            status,
            state,
            turn,
            ..
        } => {
            if status == "duplicate" {
                let state = state.as_deref().unwrap_or("unknown");
                println!(
                    "{}",
                    ta("cli-chat-duplicate", &[("state", state)], "duplicate")
                );
            } else if turn.as_deref() == Some("steered") {
                println!("{}", ta("cli-chat-steered", &[], "steered"));
            }
        }
        Frame::Other(value) => {
            // Cron results and other session events carry their text in
            // one of a few fields; show them instead of dropping them.
            let kind = value["type"].as_str().unwrap_or("event");
            let text = ["output", "message", "content"]
                .iter()
                .find_map(|key| value[*key].as_str());
            if let Some(text) = text
                && !matches!(kind, "agent_start" | "agent_end" | "history_trimmed")
            {
                println!(
                    "\n{}",
                    ta("cli-chat-event", &[("kind", kind), ("text", text)], "event")
                );
            }
        }
        Frame::Thinking { .. } | Frame::ToolResult { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_lines_map_to_commands_and_messages() {
        assert_eq!(parse_input("/quit"), Input::Quit);
        assert_eq!(parse_input(" /exit "), Input::Quit);
        assert_eq!(parse_input("/cancel"), Input::Cancel);
        assert_eq!(parse_input("   "), Input::Nothing);
        assert_eq!(parse_input(" hello "), Input::Message("hello"));
    }

    #[test]
    fn only_yes_or_always_approve() {
        assert_eq!(parse_decision("y"), Decision::Approve);
        assert_eq!(parse_decision("YES"), Decision::Approve);
        assert_eq!(parse_decision("a"), Decision::Always);
        assert_eq!(parse_decision(""), Decision::Deny);
        assert_eq!(parse_decision("sure"), Decision::Deny);
    }

    #[test]
    fn usage_line_needs_a_prompt_size() {
        assert!(usage_line(None, Some(200_000), Some(0.01)).is_none());
    }

    #[test]
    fn the_local_gateway_url_uses_a_reachable_host_and_the_prefix() {
        let mut config = Config::default();
        config.gateway.host = "0.0.0.0".into();
        config.gateway.port = 42617;
        assert_eq!(local_gateway_url(&config), "ws://127.0.0.1:42617");
        config.gateway.path_prefix = Some("/zc".into());
        assert_eq!(local_gateway_url(&config), "ws://127.0.0.1:42617/zc");
    }
}
