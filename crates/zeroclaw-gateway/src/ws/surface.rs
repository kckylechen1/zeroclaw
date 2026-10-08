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
    Ok(requested.or(query))
}

pub(super) fn connect_value(
    query: Option<ChatSurface>,
    frame: &serde_json::Value,
) -> Result<Option<ChatSurface>, ()> {
    match frame.get("surface") {
        Some(value) => resolve(query, Some(value.as_str().ok_or(())?)),
        None => Ok(query),
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
                ChatSurface::Web => "Web: use readable Markdown; keep the initial reply concise and expand when asked.",
                ChatSurface::Telegram => "Telegram: keep replies concise; use plain text with simple emphasis and short lists, avoiding tables.",
                ChatSurface::Cli => "CLI: use concise terminal-friendly text; simple Markdown is acceptable, avoiding wide tables.",
            };
            format!("## Surface\n\n{line}")
        }))
    }
}
