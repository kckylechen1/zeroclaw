use crate::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher, XmlToolDispatcher};
use anyhow::Result;
use zeroclaw_config::schema::Config;
use zeroclaw_providers::{self, ModelProvider};

pub fn build_session_model_provider(
    config: &Config,
    model_provider_ref: &str,
    model_override: Option<&str>,
) -> Result<(Box<dyn ModelProvider>, String, String)> {
    let (model_provider_name, model_provider_alias) = model_provider_ref
        .split_once('.')
        .map(|(t, a)| (t.to_string(), a.to_string()))
        .ok_or_else(|| {
            anyhow::Error::msg(format!(
                "model_provider reference `{model_provider_ref}` must be `<type>.<alias>`"
            ))
        })?;

    let entry = config
        .providers
        .models
        .find(&model_provider_name, &model_provider_alias);
    let model_name = model_override
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| {
            entry
                .and_then(|e| e.model.as_deref())
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            anyhow::Error::msg(format!(
                "model_provider `{model_provider_ref}` has no `model` configured and no model \
                 override was supplied"
            ))
        })?;

    let model_provider_runtime_options = zeroclaw_providers::provider_runtime_options_for_alias(
        config,
        &model_provider_name,
        &model_provider_alias,
    );

    let model_provider = zeroclaw_providers::create_routed_model_provider_with_options(
        config,
        model_provider_ref,
        entry.and_then(|e| e.api_key.as_deref()),
        entry.and_then(|e| e.uri.as_deref()),
        &config.reliability,
        &config.model_routes,
        &model_name,
        &model_provider_runtime_options,
    )?;

    Ok((model_provider, model_provider_name, model_name))
}

/// Resolve the tool dispatcher with the same provider-capability fallback
/// used by fresh agent construction.
#[must_use]
pub fn tool_dispatcher_for_provider(
    agent_cfg: &zeroclaw_config::schema::AliasedAgentConfig,
    model_provider: &dyn ModelProvider,
) -> Box<dyn ToolDispatcher> {
    match agent_cfg.resolved.tool_dispatcher.as_str() {
        "native" => Box::new(NativeToolDispatcher),
        "xml" => Box::new(XmlToolDispatcher),
        _ if model_provider.supports_native_tools() => Box::new(NativeToolDispatcher),
        _ => Box::new(XmlToolDispatcher),
    }
}
