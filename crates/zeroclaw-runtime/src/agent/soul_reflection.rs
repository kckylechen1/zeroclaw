//! Daemon wiring for the governed companion reflection.

use zeroclaw_config::schema::Config;
use zeroclaw_infra::session_backend::SessionBackend;
pub use zeroclaw_memory::companion::reflection::*;
use zeroclaw_memory::companion::{SoulProfileStore, SoulReflectionReceipt, UserModelStore};

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Build the agent's own model provider and model name for the reflection
/// call, resolved exactly as for a normal turn.
fn reflection_provider(config: &Config, agent_alias: &str) -> anyhow::Result<ReflectionModel> {
    let Some((provider_name, provider_alias, entry)) =
        config.resolved_model_provider_for_agent(agent_alias)
    else {
        anyhow::bail!("agents.{agent_alias}.model_provider does not resolve");
    };
    let Some(model) = entry
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    else {
        anyhow::bail!("agents.{agent_alias}.model_provider has no model");
    };
    let options = zeroclaw_providers::provider_runtime_options_for_alias(
        config,
        provider_name,
        provider_alias,
    );
    let provider = zeroclaw_providers::create_routed_model_provider_with_options(
        config,
        &format!("{provider_name}.{provider_alias}"),
        entry.api_key.as_deref(),
        entry.uri.as_deref(),
        &config.reliability,
        &config.model_routes,
        model,
        &options,
    )?;
    Ok((provider, model.to_string()))
}

/// One cadence check for one agent: start the clock, skip, or reflect.
async fn tick_agent(
    config: &Config,
    store: &SoulProfileStore,
    backend: &dyn SessionBackend,
    agent_alias: &str,
    now_unix: u64,
) -> anyhow::Result<()> {
    let since_unix = match reflection_due(store.last_reflection(agent_alias)?.as_ref(), now_unix) {
        ReflectionDue::NotYet => return Ok(()),
        ReflectionDue::StartClock => {
            store.record_reflection(
                agent_alias,
                &SoulReflectionReceipt {
                    period_from_unix: now_unix,
                    messages_read: 0,
                    proposals_created: 0,
                    user_model_candidates_created: 0,
                    outcome: OUTCOME_CLOCK_STARTED.to_string(),
                    ran_at_unix: now_unix,
                },
            )?;
            return Ok(());
        }
        ReflectionDue::Due { since_unix } => since_unix,
    };
    let messages = collect_owner_messages(
        backend,
        agent_alias,
        config.agents.len() == 1,
        &config.companion_memory.owner.gate(),
        since_unix,
        now_unix,
    );
    let user_model = UserModelStore::shared(&config.data_dir)?;
    let receipt = reflect(
        store,
        agent_alias,
        &user_model,
        || {
            Some(
                config
                    .persona_for_agent(agent_alias)
                    .copied()
                    .unwrap_or_default(),
            )
        },
        &messages,
        || reflection_provider(config, agent_alias),
        since_unix,
        now_unix,
    )
    .await?;
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_category(::zeroclaw_log::EventCategory::Agent)
            .with_attrs(::serde_json::json!({
                "agent": agent_alias,
                "messages_read": receipt.messages_read,
                "proposals_created": receipt.proposals_created,
                "outcome": receipt.outcome,
            })),
        "Soul reflection ran"
    );
    Ok(())
}

/// Daemon worker: check every [`REFLECTION_CHECK_INTERVAL`] whether any
/// configured agent is due a reflection. Returns when `cancel` fires.
pub async fn run_worker(
    config: Config,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(REFLECTION_CHECK_INTERVAL);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        if config.agents.is_empty() || !config.data_dir.is_dir() {
            continue;
        }
        let store = SoulProfileStore::shared(&config.data_dir)?;
        let backend = zeroclaw_infra::make_session_backend(
            &config.data_dir,
            &config.channels.session_backend,
        )?;
        let mut aliases: Vec<&String> = config.agents.keys().collect();
        aliases.sort();
        for alias in aliases {
            if let Err(err) = tick_agent(&config, &store, backend.as_ref(), alias, now_unix()).await
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "agent": alias,
                            "error": err.to_string(),
                        })),
                    "Soul reflection check failed"
                );
            }
        }
    }
}
