//! Per-turn presentation only. The shared Agent never captures a client's surface.

use zeroclaw_api::chat_surface::ChatSurface;
use zeroclaw_runtime::agent::prompt::{PromptContext, PromptSection};

pub(super) const VERSION: u32 = 1;

tokio::task_local! {
    pub(super) static CURRENT: Option<ChatSurface>;
}

pub(super) fn resolve(
    query: Option<ChatSurface>,
    connect: Option<&str>,
) -> Result<Option<ChatSurface>, ()> {
    let requested = connect
        .map(|name| serde_json::from_value::<ChatSurface>(name.into()).map_err(|_| ()))
        .transpose()?;
    if query.is_some() && requested.is_some() && query != requested {
        return Err(());
    }
    let surface = requested.or(query);
    if surface == Some(ChatSurface::Voice) && !cfg!(feature = "voice-surface") {
        return Err(());
    }
    Ok(surface)
}

pub(super) fn connect_value(
    query: Option<ChatSurface>,
    frame: &serde_json::Value,
) -> Result<Option<ChatSurface>, ()> {
    match frame.get("surface") {
        Some(value) => resolve(query, Some(value.as_str().ok_or(())?)),
        None => resolve(query, None),
    }
}

pub(super) struct Section;

impl PromptSection for Section {
    fn name(&self) -> &str {
        "surface"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> anyhow::Result<String> {
        let register = CURRENT.try_with(|surface| *surface).ok().flatten();
        Ok(register.map_or_else(String::new, |surface| {
            let line = match surface {
                ChatSurface::Voice => return String::new(),
                ChatSurface::Web => "Web: use readable Markdown; keep the initial reply concise and expand when asked.",
                ChatSurface::Telegram => "Telegram: keep replies concise; use plain text with simple emphasis and short lists, avoiding tables.",
                ChatSurface::Cli => "CLI: use concise terminal-friendly text; simple Markdown is acceptable, avoiding wide tables.",
            };
            format!("## Surface\n\n{line}")
        }))
    }
}

/// Transform a private outbound copy, never shared history or replay storage.
/// Keep empty chunk envelopes so turn-control sequence numbers stay contiguous.
pub(super) fn wire(state: &super::AppState, surface: Option<ChatSurface>, text: &str) -> String {
    if surface != Some(ChatSurface::Voice) {
        return text.to_owned();
    }
    let Ok(mut frame) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_owned();
    };
    render_frame(&mut frame, &state.config.read().security.leak_detection);
    frame.to_string()
}

fn render_frame(
    frame: &mut serde_json::Value,
    policy: &zeroclaw_config::schema::LeakDetectionConfig,
) {
    match frame["type"].as_str() {
        Some("chunk") => frame["content"] = "".into(),
        Some("done") => {
            if let Some(text) = frame["full_response"].as_str() {
                // Decoding entities/removing markup can reconstruct a secret.
                // Scan the exact emitted bytes under the current configured policy.
                frame["full_response"] =
                    zeroclaw_runtime::security::outbound::redact_channel_outbound_leaks(
                        &zeroclaw_runtime::spoken::render(text),
                        policy,
                        zeroclaw_runtime::security::outbound::OutboundContentFormat::PlainText,
                    )
                    .into();
            }
        }
        Some("turn_event") => render_frame(&mut frame["event"], policy),
        Some("turn_replay") => {
            if let Some(events) = frame["events"].as_array_mut() {
                for event in events {
                    render_frame(event, policy);
                }
            }
        }
        _ => {}
    }
}
