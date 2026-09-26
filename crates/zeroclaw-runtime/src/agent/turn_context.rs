//! Per-turn owner context for the body agent (#380 U3).
//!
//! Two governed projections reach the system prompt of every body turn, in a
//! fixed order and each within its own byte bound:
//!
//! 1. the Soul ([`persona_projection`]), rendered in the prompt builder's
//!    persona slot. It is re-projected only when the store's revision stamp
//!    moved since the last turn, so an owner-approved change applies from the
//!    next turn of a live session while an unchanged Soul keeps the prompt
//!    bytes (and the provider's prompt cache) stable;
//! 2. the User Model owner profile ([`user_model_section`]), appended at the
//!    end of the system prompt, bounded by
//!    [`USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS`].
//!
//! Only body agents opt in (see `AgentBuilder::turn_context`). Delegated
//! workers build their own bounded prompt and never receive either section.
//!
//! [`persona_projection`]: crate::agent::persona_projection::persona_projection
//! [`USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS`]: zeroclaw_memory::companion::USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS

use std::sync::Arc;

use zeroclaw_config::schema::Config;
use zeroclaw_memory::companion::{
    ApplicabilityContext, SoulProfileStore, USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS,
    UserModelStore, project_applicable_heads,
};

use crate::agent::persona_projection::{PersonaProjection, persona_projection};

/// The Soul store's change marker for `agent_alias`, or `None` when there is
/// no readable store. Never creates the data directory.
///
/// Read it *before* projecting: a change that lands between the read and the
/// projection then shows up as a moved stamp on the next turn instead of
/// being missed.
#[must_use]
pub fn soul_revision_stamp(config: &Config, agent_alias: &str) -> Option<u64> {
    if !config.data_dir.is_dir() {
        return None;
    }
    SoulProfileStore::shared(&config.data_dir)
        .and_then(|store| store.revision_stamp(agent_alias))
        .ok()
}

/// Render the owner-profile section that applies in `applicability`.
/// A read failure logs one WARN and renders nothing for this turn. Blocking.
#[must_use]
pub fn user_model_section_blocking(
    store: &UserModelStore,
    applicability: &ApplicabilityContext,
) -> String {
    match store.active_heads(None) {
        Ok(heads) => {
            project_applicable_heads(
                heads,
                applicability,
                USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS,
            )
            .prompt_section
        }
        Err(_) => {
            warn_user_model_read_failed();
            String::new()
        }
    }
}

/// Async form of [`user_model_section_blocking`] for callers that already
/// hold a store handle (the channel orchestrator).
pub async fn user_model_section(
    store: Arc<UserModelStore>,
    applicability: ApplicabilityContext,
) -> String {
    tokio::task::spawn_blocking(move || user_model_section_blocking(&store, &applicability))
        .await
        .unwrap_or_else(|_| {
            warn_user_model_read_failed();
            String::new()
        })
}

fn warn_user_model_read_failed() {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
        "user model read failed; owner-profile projection skipped for this turn"
    );
}

/// What one turn's assembly produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssembledTurnContext {
    /// The Soul stamp this turn observed; the next turn compares against it.
    pub soul_stamp: Option<u64>,
    /// A fresh Soul projection, present only when the stamp moved.
    pub persona: Option<PersonaProjection>,
    /// The owner-profile section, empty when nothing applies.
    pub user_model_section: String,
}

/// Assemble the governed context for one turn of `agent_alias`.
///
/// `last_soul_stamp` is what the previous turn (or construction) observed.
/// All store reads run on one blocking task.
pub async fn assemble_turn_context(
    config: Arc<Config>,
    agent_alias: String,
    last_soul_stamp: Option<u64>,
    applicability: ApplicabilityContext,
) -> AssembledTurnContext {
    let assembled = tokio::task::spawn_blocking(move || {
        assemble_blocking(&config, &agent_alias, last_soul_stamp, &applicability)
    })
    .await;
    match assembled {
        Ok(assembled) => assembled,
        Err(_) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "turn context assembly task failed; keeping the previous Soul projection"
            );
            AssembledTurnContext {
                soul_stamp: last_soul_stamp,
                ..AssembledTurnContext::default()
            }
        }
    }
}

fn assemble_blocking(
    config: &Config,
    agent_alias: &str,
    last_soul_stamp: Option<u64>,
    applicability: &ApplicabilityContext,
) -> AssembledTurnContext {
    let soul_stamp = soul_revision_stamp(config, agent_alias);
    let persona = (soul_stamp != last_soul_stamp).then(|| persona_projection(config, agent_alias));
    // Like the Soul, the prompt path never creates the data directory.
    let user_model_section = if config.data_dir.is_dir() {
        match UserModelStore::shared(&config.data_dir) {
            Ok(store) => user_model_section_blocking(&store, applicability),
            Err(_) => {
                warn_user_model_read_failed();
                String::new()
            }
        }
    } else {
        String::new()
    };
    AssembledTurnContext {
        soul_stamp,
        persona,
        user_model_section,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_memory::companion::{SoulIdentity, UserModelKind};

    fn config_in(dir: &std::path::Path) -> Arc<Config> {
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        Arc::new(Config {
            data_dir,
            ..Config::default()
        })
    }

    fn ctx() -> ApplicabilityContext {
        ApplicabilityContext::new("nova", "wss", "gw_s1")
    }

    #[tokio::test]
    async fn soul_is_reprojected_only_when_its_revision_moves() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let stamp = soul_revision_stamp(&config, "nova");
        let _ = persona_projection(&config, "nova");

        // Seeding on first projection moves the stamp once.
        let first = assemble_turn_context(config.clone(), "nova".into(), stamp, ctx()).await;
        let second =
            assemble_turn_context(config.clone(), "nova".into(), first.soul_stamp, ctx()).await;
        assert!(
            second.persona.is_none(),
            "unchanged Soul must not re-project"
        );
        assert_eq!(second.soul_stamp, first.soul_stamp);

        let store = SoulProfileStore::shared(&config.data_dir).unwrap();
        let head = store.profile("nova").unwrap().identity.unwrap().revision;
        store
            .set_identity(
                "nova",
                SoulIdentity {
                    name: "Nova Prime".into(),
                    self_description: None,
                    primary_language: None,
                    pronouns: None,
                },
                head,
                10,
            )
            .unwrap();
        let third =
            assemble_turn_context(config.clone(), "nova".into(), second.soul_stamp, ctx()).await;
        let section = third.persona.unwrap().section.unwrap();
        assert!(section.contains("You are Nova Prime."), "{section}");
    }

    #[tokio::test]
    async fn user_model_section_carries_applicable_heads_only() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let store = UserModelStore::shared(&config.data_dir).unwrap();
        store
            .record_owner_statement(
                UserModelKind::Preference,
                "Answer in short paragraphs.",
                "style.length",
                "global",
                1,
            )
            .unwrap();
        store
            .record_owner_statement(
                UserModelKind::Goal,
                "Ship the trading bot.",
                "goal.trading",
                "session:elsewhere",
                2,
            )
            .unwrap();
        let assembled = assemble_turn_context(config, "nova".into(), None, ctx()).await;
        let section = assembled.user_model_section;
        assert!(section.starts_with("## Owner profile (authoritative)\n"));
        assert!(section.contains("Answer in short paragraphs."), "{section}");
        assert!(!section.contains("trading"), "{section}");
        assert!(section.len() <= USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS);
    }

    #[tokio::test]
    async fn missing_data_dir_assembles_nothing_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config {
            data_dir: dir.path().join("absent"),
            ..Config::default()
        });
        let assembled = assemble_turn_context(config.clone(), "nova".into(), None, ctx()).await;
        assert_eq!(assembled, AssembledTurnContext::default());
        assert!(!config.data_dir.exists());
    }

    /// Records the system prompt of every model call.
    struct CapturingProvider {
        seen: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl zeroclaw_providers::ModelProvider for CapturingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            request: zeroclaw_providers::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<zeroclaw_providers::ChatResponse> {
            let system = request
                .messages
                .iter()
                .find(|message| message.role == "system")
                .map(|message| message.content.clone())
                .unwrap_or_default();
            self.seen.lock().push(system);
            Ok(zeroclaw_providers::ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for CapturingProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "capturing"
        }
    }

    /// #380 U3: a body agent kept alive across turns picks up an approved Soul
    /// proposal on its next turn, keeps the prompt byte-identical while nothing
    /// changed, and carries the User Model owner profile at the end.
    #[tokio::test]
    async fn approved_soul_proposal_changes_the_next_turns_prompt_bytes() {
        use zeroclaw_memory::companion::{
            GrowthKind, NewSoulProposal, SoulProfileStore, SoulProposalLayer, SoulProposalOutcome,
            SoulProposalResolution, UserModelKind, UserModelStore,
        };

        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config = Arc::new(Config {
            data_dir: data_dir.clone(),
            ..Config::default()
        });
        UserModelStore::shared(&data_dir)
            .unwrap()
            .record_owner_statement(
                UserModelKind::Preference,
                "Keep answers short.",
                "style.length",
                "global",
                1,
            )
            .unwrap();

        let soul_stamp = crate::agent::turn_context::soul_revision_stamp(&config, "nova");
        let persona = crate::agent::persona_projection::persona_projection(&config, "nova");
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let memory_cfg = zeroclaw_config::schema::MemoryConfig {
            backend: "none".into(),
            ..zeroclaw_config::schema::MemoryConfig::default()
        };
        let mem: Arc<dyn zeroclaw_memory::Memory> =
            Arc::from(zeroclaw_memory::create_memory(&memory_cfg, dir.path(), None).unwrap());
        let mut agent = crate::agent::agent::Agent::builder()
            .model_provider(Box::new(CapturingProvider {
                seen: Arc::clone(&seen),
            }))
            .tools(vec![])
            .memory(mem)
            .observer(Arc::from(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(dir.path().to_path_buf())
            .prompt_builder(crate::agent::prompt::SystemPromptBuilder::with_persona(
                persona,
            ))
            .provider_switch_config(crate::agent::agent::ProviderSwitchConfig {
                config: Some(Arc::clone(&config)),
            })
            .agent_alias("nova".into())
            .governed_turn_context(soul_stamp)
            .build()
            .unwrap();

        let system_of_call = |index: usize| -> String {
            let calls = seen.lock();
            calls[index].clone()
        };

        agent.turn("hello").await.unwrap();
        agent.turn("again").await.unwrap();
        let first = system_of_call(0);
        let second = system_of_call(1);
        assert_eq!(
            first, second,
            "an unchanged Soul must keep the prompt bytes"
        );
        assert!(first.contains("You are nova."), "{first}");
        assert!(
            first
                .trim_end()
                .ends_with("- preference: Keep answers short. [scope: global]"),
            "owner profile must close the system prompt: {first}"
        );

        let store = SoulProfileStore::shared(&data_dir).unwrap();
        let SoulProposalOutcome::Recorded { id } = store
            .submit_proposal(
                "nova",
                NewSoulProposal {
                    layer: SoulProposalLayer::Growth,
                    proposal: "We call a bad trade a paper cut.".into(),
                    growth_kind: Some(GrowthKind::Bond),
                    ..NewSoulProposal::default()
                },
                2,
            )
            .unwrap()
        else {
            panic!("proposal must be recorded");
        };
        store
            .resolve_proposal("nova", id, SoulProposalResolution::Accepted, None, None, 3)
            .unwrap();

        agent.turn("and now").await.unwrap();
        let third = system_of_call(2);
        assert_ne!(third, second);
        assert!(
            third.contains("- Between us: We call a bad trade a paper cut."),
            "{third}"
        );
        // Only the Soul section moved: the prefix before it is unchanged.
        let growth_at = third.find("## Who I've become").unwrap();
        assert_eq!(&third[..growth_at], &second[..growth_at]);
    }

    /// Builder-made agents without a governed turn context keep a fixed prompt
    /// and never read the Soul or User Model stores (delegated workers and tests).
    #[tokio::test]
    async fn agents_without_turn_context_get_no_owner_sections() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config = Arc::new(Config {
            data_dir: data_dir.clone(),
            ..Config::default()
        });
        zeroclaw_memory::companion::UserModelStore::shared(&data_dir)
            .unwrap()
            .record_owner_statement(
                zeroclaw_memory::companion::UserModelKind::Preference,
                "Keep answers short.",
                "style.length",
                "global",
                1,
            )
            .unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let memory_cfg = zeroclaw_config::schema::MemoryConfig {
            backend: "none".into(),
            ..zeroclaw_config::schema::MemoryConfig::default()
        };
        let mem: Arc<dyn zeroclaw_memory::Memory> =
            Arc::from(zeroclaw_memory::create_memory(&memory_cfg, dir.path(), None).unwrap());
        let mut agent = crate::agent::agent::Agent::builder()
            .model_provider(Box::new(CapturingProvider {
                seen: Arc::clone(&seen),
            }))
            .tools(vec![])
            .memory(mem)
            .observer(Arc::from(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(dir.path().to_path_buf())
            .provider_switch_config(crate::agent::agent::ProviderSwitchConfig {
                config: Some(config),
            })
            .agent_alias("nova".into())
            .build()
            .unwrap();
        agent.turn("hello").await.unwrap();
        let system = seen.lock()[0].clone();
        assert!(!system.contains("Owner profile"), "{system}");
        assert!(!system.contains("## Identity"), "{system}");
        assert!(!data_dir.join("soul.db").exists());
    }
}
