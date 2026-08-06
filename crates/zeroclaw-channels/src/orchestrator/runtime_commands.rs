//! Runtime channel commands: parsing `/model`, `/models`, `/config`, `/thinking`,
//! `/new`, `/clear` and building help/config responses.
//!
//! Extracted from `orchestrator/mod.rs` so command vocabulary and pure response
//! builders can evolve independently of `ChannelRuntimeContext` dispatch.

use std::fmt::Write;
use std::path::Path;

use serde::Deserialize;
use zeroclaw_config::scattered_types::ThinkingLevel;
use zeroclaw_config::schema::Config;

use super::ChannelRouteSelection;

const MODEL_CACHE_FILE: &str = "models_cache.json";
const MODEL_CACHE_PREVIEW_LIMIT: usize = 10;

/// Selectable scope for a session-only `/model` override. The absence of any
/// stored entry is the implicit "default" (config) tier, so it is not a variant.
/// Precedence at resolution time is `User > Agent` (above the per-sender
/// route override and the config default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverrideScope {
    /// All chats for the invoking user under this bot alias (drops thread).
    User,
    /// The whole agent, everywhere (drops the sender).
    Agent,
}

pub(crate) fn channel_runtime_cli_string(key: &str) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string(key)
}

pub(crate) fn channel_runtime_cli_string_with_args(key: &str, args: &[(&str, &str)]) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string_with_args(key, args)
}

pub(crate) fn channel_runtime_scope_label(scope: OverrideScope) -> String {
    match scope {
        OverrideScope::User => channel_runtime_cli_string("channel-runtime-scope-user"),
        OverrideScope::Agent => channel_runtime_cli_string("channel-runtime-scope-agent"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelRuntimeCommand {
    ShowProviders,
    SetProvider(String),
    ShowModel,
    SetModel(String),
    /// `/model --user|--agent <ref>` — set the model at an explicit scope.
    SetModelScoped(OverrideScope, String),
    ShowConfig,
    NewSession,
    SetThinking(Option<ThinkingLevel>),
    InvalidThinking(String),
}

pub(crate) fn supports_runtime_model_switch(channel_name: &str) -> bool {
    matches!(
        channel_name,
        "telegram"
            | "discord"
            | "matrix"
            | "slack"
            | "wecom_ws"
            | "whatsapp"
            | "whatsapp-web"
            | "whatsapp_web"
    )
}

pub(crate) fn parse_thinking_command_arg(
    raw: Option<&str>,
) -> Result<Option<ThinkingLevel>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let token = raw.trim();
    if token.is_empty() {
        return Ok(None);
    }
    match token.to_ascii_lowercase().as_str() {
        "reset" | "default" | "auto" => Ok(None),
        "on" | "true" | "1" | "enable" | "enabled" | "yes" => Ok(Some(ThinkingLevel::High)),
        "off" | "false" | "0" | "disable" | "disabled" | "no" => Ok(Some(ThinkingLevel::Off)),
        _ => ThinkingLevel::from_str_insensitive(token)
            .map(Some)
            .ok_or_else(|| token.to_string()),
    }
}

pub(crate) fn parse_runtime_command(
    channel_name: &str,
    content: &str,
) -> Option<ChannelRuntimeCommand> {
    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command_token = parts.next()?;
    let base_command = command_token
        .split('@')
        .next()
        .unwrap_or(command_token)
        .to_ascii_lowercase();

    match base_command.as_str() {
        // `/new` and bare `/clear` are available on every channel — no model-switch gate.
        "/new" => Some(ChannelRuntimeCommand::NewSession),
        "/clear" => {
            if parts.next().is_none() {
                Some(ChannelRuntimeCommand::NewSession)
            } else {
                None
            }
        }
        "/thinking" => {
            let arg = parts.next();
            if parts.next().is_some() {
                Some(ChannelRuntimeCommand::InvalidThinking(
                    "too many arguments".to_string(),
                ))
            } else {
                match parse_thinking_command_arg(arg) {
                    Ok(level) => Some(ChannelRuntimeCommand::SetThinking(level)),
                    Err(raw) => Some(ChannelRuntimeCommand::InvalidThinking(raw)),
                }
            }
        }
        // Model/model_provider switching is channel-gated.
        "/models" if supports_runtime_model_switch(channel_name) => {
            if let Some(model_provider) = parts.next() {
                Some(ChannelRuntimeCommand::SetProvider(
                    model_provider.trim().to_string(),
                ))
            } else {
                Some(ChannelRuntimeCommand::ShowProviders)
            }
        }
        "/model" if supports_runtime_model_switch(channel_name) => {
            let rest: Vec<&str> = parts.collect();
            // An optional leading `--user|--agent` flag selects the override
            // scope; without it, bare `/model <ref>` keeps its existing
            // per-sender behavior.
            let (scope, model_tokens) = match rest.first() {
                Some(&"--user") => (Some(OverrideScope::User), &rest[1..]),
                Some(&"--agent") => (Some(OverrideScope::Agent), &rest[1..]),
                // A mistyped `--flag` is a typo, not a model id — don't silently
                // set a model literally named "--foo". Show the help/ladder.
                Some(t) if t.starts_with("--") => return Some(ChannelRuntimeCommand::ShowModel),
                _ => (None, &rest[..]),
            };
            let model = model_tokens.join(" ").trim().to_string();
            match (scope, model.is_empty()) {
                // `/model` or `/model --scope` (no ref): show current + scopes.
                (_, true) => Some(ChannelRuntimeCommand::ShowModel),
                (None, false) => Some(ChannelRuntimeCommand::SetModel(model)),
                (Some(scope), false) => Some(ChannelRuntimeCommand::SetModelScoped(scope, model)),
            }
        }
        "/config" if supports_runtime_model_switch(channel_name) => {
            Some(ChannelRuntimeCommand::ShowConfig)
        }
        _ => None,
    }
}

pub(crate) fn canonical_model_provider_name(name: &str) -> Option<String> {
    let candidate = name.trim();
    if candidate.is_empty() {
        return None;
    }

    zeroclaw_providers::list_model_providers()
        .into_iter()
        .find(|model_provider| model_provider.name.eq_ignore_ascii_case(candidate))
        .map(|model_provider| model_provider.name.to_string())
}

/// Outcome of resolving a `/models <arg>` request to a configured,
/// alias-backed provider ref. The bare family path must never construct a
/// provider that ignores the configured `[providers.models.<family>.<alias>]`
/// key/URI — every accepted route resolves to a real alias entry.
#[cfg_attr(test, derive(Debug))]
pub(crate) enum ModelsCommandResolution {
    /// A dotted `<family>.<alias>` ref backed by a configured entry.
    Resolved(String),
    /// The family is valid but has more than one configured alias; the user
    /// must qualify which one. Carries the canonical family and its aliases.
    Ambiguous {
        family: String,
        aliases: Vec<String>,
    },
    /// The family is valid but has no configured alias entry, so there is no
    /// credentialed provider to switch to.
    NoAlias(String),
    /// The argument names no known provider family.
    Unknown,
}

pub(crate) fn resolve_models_command(
    config: &zeroclaw_config::schema::Config,
    raw: &str,
) -> ModelsCommandResolution {
    let candidate = raw.trim();
    if let Some((family, alias)) = candidate.split_once('.') {
        return match config.providers.models.find(family, alias) {
            Some(_) => ModelsCommandResolution::Resolved(format!("{family}.{alias}")),
            None => ModelsCommandResolution::NoAlias(candidate.to_string()),
        };
    }

    let Some(family) = canonical_model_provider_name(candidate) else {
        return ModelsCommandResolution::Unknown;
    };

    let mut aliases: Vec<String> = config
        .providers
        .models
        .aliases_of(&family)
        .map(ToString::to_string)
        .collect();
    aliases.sort();
    match aliases.len() {
        0 => ModelsCommandResolution::NoAlias(family),
        1 => ModelsCommandResolution::Resolved(format!("{family}.{}", aliases[0])),
        _ => ModelsCommandResolution::Ambiguous { family, aliases },
    }
}

pub(crate) fn resolve_provider_ref_for_runtime_switch(
    config: &Config,
    raw: &str,
) -> anyhow::Result<String> {
    match resolve_models_command(config, raw) {
        ModelsCommandResolution::Resolved(provider_ref) => Ok(provider_ref),
        ModelsCommandResolution::Ambiguous { family, aliases } => {
            let list = aliases
                .iter()
                .map(|alias| format!("{family}.{alias}"))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "model_provider `{family}` has multiple configured aliases; use one of: {list}"
            )
        }
        ModelsCommandResolution::NoAlias(ref_or_family) => {
            anyhow::bail!(
                "model_provider `{ref_or_family}` does not resolve to a configured provider"
            )
        }
        ModelsCommandResolution::Unknown => {
            anyhow::bail!("unknown model_provider `{raw}`")
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ModelCacheState {
    entries: Vec<ModelCacheEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ModelCacheEntry {
    model_provider: String,
    models: Vec<String>,
}

pub(crate) fn load_cached_model_preview(workspace_dir: &Path, provider_name: &str) -> Vec<String> {
    let cache_path = workspace_dir.join("state").join(MODEL_CACHE_FILE);
    let Ok(raw) = std::fs::read_to_string(cache_path) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<ModelCacheState>(&raw) else {
        return Vec::new();
    };

    state
        .entries
        .into_iter()
        .find(|entry| entry.model_provider == provider_name)
        .map(|entry| {
            entry
                .models
                .into_iter()
                .take(MODEL_CACHE_PREVIEW_LIMIT)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(crate) fn build_models_help_response(
    current: &ChannelRouteSelection,
    workspace_dir: &Path,
    model_routes: &[zeroclaw_config::schema::ModelRouteConfig],
) -> String {
    let mut response = String::new();
    response.push_str(&channel_runtime_cli_string_with_args(
        "channel-runtime-current-model-status",
        &[
            ("provider", current.model_provider.as_str()),
            ("model", current.model.as_str()),
        ],
    ));
    response.push('\n');
    response.push_str(&channel_runtime_cli_string(
        "channel-runtime-model-switch-hint",
    ));
    response.push('\n');

    if !model_routes.is_empty() {
        response.push('\n');
        response.push_str(&channel_runtime_cli_string(
            "channel-runtime-configured-routes-header",
        ));
        response.push('\n');
        for route in model_routes {
            let _ = writeln!(
                response,
                "  `{}` → {} ({})",
                route.hint, route.model, route.model_provider
            );
        }
    }

    let cached_models = load_cached_model_preview(workspace_dir, &current.model_provider);
    if cached_models.is_empty() {
        response.push('\n');
        response.push_str(&channel_runtime_cli_string_with_args(
            "channel-runtime-no-cached-models",
            &[("provider", current.model_provider.as_str())],
        ));
        response.push('\n');
    } else {
        response.push('\n');
        response.push_str(&channel_runtime_cli_string_with_args(
            "channel-runtime-cached-model-ids-header",
            &[("count", &cached_models.len().to_string())],
        ));
        response.push('\n');
        for model in cached_models {
            let _ = writeln!(response, "- `{model}`");
        }
    }

    response
}

pub(crate) fn build_providers_help_response(current: &ChannelRouteSelection) -> String {
    let mut response = String::new();
    response.push_str(&channel_runtime_cli_string_with_args(
        "channel-runtime-current-model-status",
        &[
            ("provider", current.model_provider.as_str()),
            ("model", current.model.as_str()),
        ],
    ));
    response.push('\n');
    response.push_str(&channel_runtime_cli_string(
        "channel-runtime-provider-switch-hint",
    ));
    response.push('\n');
    response.push_str(&channel_runtime_cli_string(
        "channel-runtime-model-switch-hint",
    ));
    response.push_str("\n\n");
    response.push_str(&channel_runtime_cli_string(
        "channel-runtime-available-providers-header",
    ));
    response.push('\n');
    for model_provider in zeroclaw_providers::list_model_providers() {
        let _ = writeln!(response, "- {}", model_provider.name);
    }
    response
}

/// Build a plain-text `/config` response for non-Slack channels.
pub(crate) fn build_config_text_response(
    current: &ChannelRouteSelection,
    _workspace_dir: &Path,
    model_routes: &[zeroclaw_config::schema::ModelRouteConfig],
) -> String {
    let mut resp = String::new();
    resp.push_str(&channel_runtime_cli_string_with_args(
        "channel-runtime-current-model-status",
        &[
            ("provider", current.model_provider.as_str()),
            ("model", current.model.as_str()),
        ],
    ));
    resp.push('\n');
    resp.push('\n');
    resp.push_str(&channel_runtime_cli_string(
        "channel-runtime-available-providers-header",
    ));
    resp.push('\n');
    for p in zeroclaw_providers::list_model_providers() {
        let _ = writeln!(resp, "- `{}`", p.name);
    }
    if !model_routes.is_empty() {
        resp.push('\n');
        resp.push_str(&channel_runtime_cli_string(
            "channel-runtime-configured-routes-header",
        ));
        resp.push('\n');
        for route in model_routes {
            let _ = writeln!(
                resp,
                "  `{}` -> {} ({})",
                route.hint, route.model, route.model_provider
            );
        }
    }
    resp.push('\n');
    resp.push_str(&channel_runtime_cli_string(
        "channel-runtime-config-switch-hints",
    ));
    resp
}

/// Build a Slack Block Kit JSON payload for the `/config` interactive UI.
pub(crate) fn build_config_block_kit(
    current: &ChannelRouteSelection,
    workspace_dir: &Path,
    model_routes: &[zeroclaw_config::schema::ModelRouteConfig],
) -> String {
    let provider_options: Vec<serde_json::Value> = zeroclaw_providers::list_model_providers()
        .iter()
        .map(|p| {
            serde_json::json!({
                "text": { "type": "plain_text", "text": p.display_name },
                "value": p.name
            })
        })
        .collect();

    // Build model options from model_routes + cached models.
    let mut model_options: Vec<serde_json::Value> = model_routes
        .iter()
        .map(|r| {
            let label = if r.hint.is_empty() {
                r.model.clone()
            } else {
                format!("{} ({})", r.model, r.hint)
            };
            serde_json::json!({
                "text": { "type": "plain_text", "text": label },
                "value": r.model
            })
        })
        .collect();

    let cached = load_cached_model_preview(workspace_dir, &current.model_provider);
    for model_id in cached {
        if !model_options.iter().any(|o| {
            o.get("value")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v == model_id)
        }) {
            model_options.push(serde_json::json!({
                "text": { "type": "plain_text", "text": model_id },
                "value": model_id
            }));
        }
    }

    // If the current model is not in the list, prepend it.
    if !model_options.iter().any(|o| {
        o.get("value")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == current.model)
    }) {
        model_options.insert(
            0,
            serde_json::json!({
                "text": { "type": "plain_text", "text": &current.model },
                "value": &current.model
            }),
        );
    }

    // Find initial options matching current selection.
    let initial_provider = provider_options
        .iter()
        .find(|o| {
            o.get("value")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v == current.model_provider)
        })
        .cloned();

    let initial_model = model_options
        .iter()
        .find(|o| {
            o.get("value")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v == current.model)
        })
        .cloned();

    let mut provider_select = serde_json::json!({
        "type": "static_select",
        "action_id": "zeroclaw_config_provider",
        "placeholder": {
            "type": "plain_text",
            "text": channel_runtime_cli_string("channel-runtime-config-select-provider-placeholder")
        },
        "options": provider_options
    });
    if let Some(init) = initial_provider {
        provider_select["initial_option"] = init;
    }

    let mut model_select = serde_json::json!({
        "type": "static_select",
        "action_id": "zeroclaw_config_model",
        "placeholder": {
            "type": "plain_text",
            "text": channel_runtime_cli_string("channel-runtime-config-select-model-placeholder")
        },
        "options": model_options
    });
    if let Some(init) = initial_model {
        model_select["initial_option"] = init;
    }

    let blocks = serde_json::json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": channel_runtime_cli_string_with_args(
                    "channel-runtime-config-block-title",
                    &[
                        ("provider", current.model_provider.as_str()),
                        ("model", current.model.as_str()),
                    ],
                )
            }
        },
        {
            "type": "section",
            "block_id": "config_provider_block",
            "text": {
                "type": "mrkdwn",
                "text": channel_runtime_cli_string("channel-runtime-config-provider-label")
            },
            "accessory": provider_select
        },
        {
            "type": "section",
            "block_id": "config_model_block",
            "text": {
                "type": "mrkdwn",
                "text": channel_runtime_cli_string("channel-runtime-config-model-label")
            },
            "accessory": model_select
        }
    ]);

    blocks.to_string()
}
