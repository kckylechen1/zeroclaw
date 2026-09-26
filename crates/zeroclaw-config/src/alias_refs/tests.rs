use super::*;
use crate::multi_agent::{AccessMode, AgentAlias, PeerGroupConfig};
use crate::schema::{AliasedAgentConfig, Config, EmbeddingRouteConfig, ModelRouteConfig};

/// Empty config with the alias-keyed containers cleared so Config::default
/// can't inject spurious references into assertions.
fn empty_config() -> Config {
    let mut c = Config::default();
    c.agents.clear();
    c.peer_groups.clear();
    c.model_routes.clear();
    c.embedding_routes.clear();
    c.escalation.alert_channels.clear();
    c.heartbeat.enabled = false;
    c.heartbeat.agent.clear();
    c.acp.default_agent = None;
    c
}

fn provider_kind(family: &str) -> AliasKind {
    AliasKind::Provider {
        category: ProviderCategory::Models,
        family: family.to_string(),
    }
}

#[test]
fn provider_models_hard_and_soft() {
    let mut cfg = empty_config();
    cfg.agents.insert(
        "researcher".to_string(),
        AliasedAgentConfig {
            model_provider: "anthropic.default".into(),
            ..Default::default()
        },
    );
    cfg.agents.insert(
        "triage".to_string(),
        AliasedAgentConfig {
            model_provider: "openai.fast".into(), // unrelated, must not match
            classifier_provider: "anthropic.default".into(),
            ..Default::default()
        },
    );
    cfg.model_routes.push(ModelRouteConfig {
        hint: "deep".to_string(),
        model_provider: "anthropic.default".to_string(),
        model: "claude".to_string(),
        api_key: None,
    });

    let kind = provider_kind("anthropic");
    let sites = find_all_references(&cfg, &kind, "default");
    assert_eq!(sites.len(), 3, "model_provider + classifier + route");

    let hard: Vec<_> = sites
        .iter()
        .filter(|s| s.strength == RefStrength::Hard)
        .collect();
    assert_eq!(hard.len(), 1);
    assert_eq!(hard[0].path, "agents.researcher.model_provider");
    assert_eq!(hard[0].action, ScrubAction::Refuse);

    let report = plan_delete(&cfg, &kind, "default");
    assert!(
        !report.allowed,
        "a hard model_provider ref must block the delete"
    );
    assert_eq!(report.blockers.len(), 1);
    assert_eq!(report.scrubs.len(), 2);
}

#[test]
fn provider_tts_is_soft_clear() {
    let mut cfg = empty_config();
    cfg.agents.insert(
        "voice".to_string(),
        AliasedAgentConfig {
            tts_provider: "elevenlabs.default".into(),
            ..Default::default()
        },
    );
    let kind = AliasKind::Provider {
        category: ProviderCategory::Tts,
        family: "elevenlabs".to_string(),
    };
    let report = plan_delete(&cfg, &kind, "default");
    assert!(report.allowed);
    assert_eq!(report.scrubs.len(), 1);
    assert_eq!(report.scrubs[0].path, "agents.voice.tts_provider");
    assert_eq!(report.scrubs[0].action, ScrubAction::ClearOptional);
}

#[test]
fn channel_hard_and_soft() {
    let mut cfg = empty_config();
    cfg.agents.insert(
        "ops".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    let group = PeerGroupConfig {
        channel: "discord.main".into(),
        ..Default::default()
    };
    cfg.peer_groups.insert("crew".to_string(), group);
    cfg.escalation
        .alert_channels
        .push("discord.main".to_string());

    let kind = AliasKind::Channel {
        channel_type: "discord".to_string(),
    };
    let report = plan_delete(&cfg, &kind, "main");
    assert_eq!(
        report.blockers.len(),
        1,
        "peer_groups channel is a hard ref"
    );
    assert_eq!(report.blockers[0].path, "peer_groups.crew.channel");
    assert_eq!(report.scrubs.len(), 2, "agent channel + alert_channel");
    assert!(!report.allowed);
}

#[test]
fn channel_bare_type_group_is_not_matched_by_alias_delete() {
    let mut cfg = empty_config();
    // bare type, not a specific alias
    let group = PeerGroupConfig {
        channel: "discord".into(),
        ..Default::default()
    };
    cfg.peer_groups.insert("crew".to_string(), group);
    let kind = AliasKind::Channel {
        channel_type: "discord".to_string(),
    };
    assert!(find_all_references(&cfg, &kind, "main").is_empty());
}

#[test]
fn agent_refs_heartbeat_hard_when_enabled() {
    let mut cfg = empty_config();
    cfg.heartbeat.enabled = true;
    cfg.heartbeat.agent = "bot".to_string();
    cfg.acp.default_agent = Some("bot".to_string());
    let mut referrer = AliasedAgentConfig {
        ..Default::default()
    };
    // workspace allowlists
    referrer
        .workspace
        .access
        .insert(AgentAlias::new("bot"), AccessMode::Read);
    referrer
        .workspace
        .read_memory_from
        .push(AgentAlias::new("bot"));
    cfg.agents.insert("lead".to_string(), referrer);
    let mut group = PeerGroupConfig::default();
    group.agents.push(AgentAlias::new("bot"));
    cfg.peer_groups.insert("crew".to_string(), group);

    let report = plan_delete(&cfg, &AliasKind::Agent, "bot");
    // heartbeat (hard) + access + read_memory_from + peer member + acp
    assert_eq!(report.blockers.len(), 1);
    assert_eq!(report.blockers[0].path, "heartbeat.agent");
    assert_eq!(report.scrubs.len(), 4);
    assert!(!report.allowed);
}

#[test]
fn agent_heartbeat_soft_when_disabled() {
    let mut cfg = empty_config();
    cfg.heartbeat.enabled = false;
    cfg.heartbeat.agent = "bot".to_string();
    let report = plan_delete(&cfg, &AliasKind::Agent, "bot");
    assert!(report.allowed, "disabled heartbeat pointer is soft");
    assert_eq!(report.scrubs.len(), 1);
    assert_eq!(report.scrubs[0].action, ScrubAction::ClearOptional);
}

#[test]
fn no_references_is_allowed_and_empty() {
    let cfg = empty_config();
    let report = plan_delete(&cfg, &provider_kind("anthropic"), "default");
    assert!(report.allowed);
    assert!(report.blockers.is_empty() && report.scrubs.is_empty());
}

#[test]
fn ref_sites_are_sorted_by_owner() {
    let mut cfg = empty_config();
    for name in ["zeta", "alpha", "mid"] {
        cfg.agents.insert(
            name.to_string(),
            AliasedAgentConfig {
                classifier_provider: "anthropic.default".into(),
                ..Default::default()
            },
        );
    }
    let sites = find_all_references(&cfg, &provider_kind("anthropic"), "default");
    let paths: Vec<_> = sites.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "agents.alpha.classifier_provider",
            "agents.mid.classifier_provider",
            "agents.zeta.classifier_provider",
        ]
    );
}

#[test]
fn whitespace_padded_provider_refs_are_found() {
    // validate() trims provider refs, so padded TOML values pass validation;
    // find_all_references must trim too or it silently misses them.
    let mut cfg = empty_config();
    cfg.agents.insert(
        "researcher".to_string(),
        AliasedAgentConfig {
            model_provider: "  anthropic.default  ".into(),
            classifier_provider: " anthropic.default ".into(),
            ..Default::default()
        },
    );
    cfg.model_routes.push(ModelRouteConfig {
        hint: "deep".to_string(),
        model_provider: " anthropic.default ".to_string(),
        model: "claude".to_string(),
        api_key: None,
    });
    let kind = provider_kind("anthropic");
    let sites = find_all_references(&cfg, &kind, "default");
    assert_eq!(
        sites.len(),
        3,
        "padded model_provider + classifier + route still found"
    );
    // raw_value preserves the actual stored (padded) text.
    let mp = sites
        .iter()
        .find(|s| s.path == "agents.researcher.model_provider")
        .unwrap();
    assert_eq!(mp.raw_value, "  anthropic.default  ");
    assert!(
        !plan_delete(&cfg, &kind, "default").allowed,
        "padded hard ref still blocks"
    );
}

#[test]
fn agent_ref_trimming_mirrors_validate() {
    let mut cfg = empty_config();
    // TRIM-matched refs (validate trims): padded values must be FOUND.
    cfg.heartbeat.enabled = false;
    cfg.heartbeat.agent = "  bot  ".to_string();
    cfg.acp.default_agent = Some(" bot ".to_string());
    cfg.agents.insert(
        "lead".to_string(),
        AliasedAgentConfig {
            ..Default::default()
        },
    );
    // RAW-matched ref (validate does NOT trim read_memory_from): a padded
    // value must NOT match, mirroring validate exactly.
    cfg.agents
        .get_mut("lead")
        .unwrap()
        .workspace
        .read_memory_from
        .push(AgentAlias::new(" bot "));

    let sites = find_all_references(&cfg, &AliasKind::Agent, "bot");
    let paths: Vec<_> = sites.iter().map(|s| s.path.as_str()).collect();
    assert!(paths.contains(&"heartbeat.agent"));
    assert!(paths.contains(&"acp.default_agent"));
    assert!(
        !paths.iter().any(|p| p.contains("read_memory_from")),
        "padded read_memory_from is raw-matched, must NOT match (mirror validate)"
    );
    let hb = sites.iter().find(|s| s.path == "heartbeat.agent").unwrap();
    assert_eq!(hb.raw_value, "  bot  ");
}

#[test]
fn provider_fallback_and_embedding_route_refs_found() {
    let mut cfg = empty_config();
    // Another provider whose fallback names the target.
    cfg.providers
        .models
        .ensure("openai", "main")
        .unwrap()
        .fallback = vec!["anthropic.default".into()];
    cfg.embedding_routes.push(EmbeddingRouteConfig {
        hint: "sem".to_string(),
        model_provider: "anthropic.default".to_string(),
        model: "emb".to_string(),
        dimensions: None,
        api_key: None,
    });
    let sites = find_all_references(&cfg, &provider_kind("anthropic"), "default");
    let paths: Vec<_> = sites.iter().map(|s| s.path.as_str()).collect();
    assert!(paths.contains(&"providers.models.openai.main.fallback[0]"));
    assert!(paths.iter().any(|p| p.starts_with("embedding_routes[")));
    assert_eq!(sites.len(), 2);
}

#[test]
fn provider_transcription_is_soft_clear() {
    let mut cfg = empty_config();
    cfg.agents.insert(
        "scribe".to_string(),
        AliasedAgentConfig {
            transcription_provider: "deepgram.default".into(),
            ..Default::default()
        },
    );
    let kind = AliasKind::Provider {
        category: ProviderCategory::Transcription,
        family: "deepgram".to_string(),
    };
    let report = plan_delete(&cfg, &kind, "default");
    assert!(report.allowed);
    assert_eq!(report.scrubs.len(), 1);
    assert_eq!(
        report.scrubs[0].path,
        "agents.scribe.transcription_provider"
    );
    assert_eq!(report.scrubs[0].action, ScrubAction::ClearOptional);
}

// ── review two delete-impact gaps ────────────────────────────────

#[test]
fn channel_delete_of_last_alias_blocks_bare_type_peer_group() {
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap(); // the ONLY discord alias
    cfg.peer_groups.insert(
        "crew".to_string(),
        PeerGroupConfig {
            channel: "discord".into(), // bare type — would dangle if discord empties
            ..Default::default()
        },
    );
    let kind = AliasKind::Channel {
        channel_type: "discord".to_string(),
    };
    // Deleting the last alias is HARD-blocked by the bare group's channel.
    let report = plan_delete(&cfg, &kind, "main");
    assert!(!report.allowed, "last-alias delete must be refused");
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.path == "peer_groups.crew.channel"),
        "{:?}",
        report.blockers
    );

    // With a SECOND alias present, deleting one is fine (the bare group still
    // has a `discord.*` to resolve against).
    cfg.create_map_key("channels.discord", "backup").unwrap();
    assert!(plan_delete(&cfg, &kind, "main").allowed);
}

#[test]
fn channel_delete_blocks_when_bare_group_member_loses_only_channel() {
    // Audacity88's case: the TYPE survives (backup remains) but a bare-group
    // MEMBER's only `<type>.*` channel is the target. Soft-scrubbing it would
    // leave the member with no discord channel → validate() fails at
    // peer_groups.crew.agents[0]. The planner must HARD-block it instead.
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    cfg.create_map_key("channels.discord", "backup").unwrap(); // type stays alive
    cfg.agents.insert(
        "bot".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into()], // bot's ONLY discord channel
            ..Default::default()
        },
    );
    let mut crew = PeerGroupConfig {
        channel: "discord".into(), // bare type
        ..Default::default()
    };
    crew.agents.push(AgentAlias::new("bot"));
    cfg.peer_groups.insert("crew".to_string(), crew);

    let kind = AliasKind::Channel {
        channel_type: "discord".to_string(),
    };
    let report = plan_delete(&cfg, &kind, "main");
    assert!(
        !report.allowed,
        "member would be orphaned — must be refused: scrubs={:?}",
        report.scrubs
    );
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.path == "peer_groups.crew.agents[0]"),
        "{:?}",
        report.blockers
    );

    // If the member also has `discord.backup`, deleting `main` keeps it a
    // member of the bare group → allowed.
    cfg.agents.get_mut("bot").unwrap().channels =
        vec!["discord.main".into(), "discord.backup".into()];
    assert!(
        plan_delete(&cfg, &kind, "main").allowed,
        "member keeps a sibling discord channel → not orphaned"
    );
}

#[test]
fn agent_delete_blocks_on_solely_owned_channel() {
    let mut cfg = empty_config();
    cfg.agents.insert(
        "bot".to_string(),
        AliasedAgentConfig {
            enabled: true,
            channels: vec!["discord.main".into()], // bot owns discord.main
            ..Default::default()
        },
    );
    // Deleting the sole enabled owner orphans the channel route → HARD block.
    let report = plan_delete(&cfg, &AliasKind::Agent, "bot");
    assert!(!report.allowed);
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.path == "agents.bot.channels[0]"),
        "{:?}",
        report.blockers
    );

    // A second enabled agent that also lists the channel keeps it owned, so
    // deleting `bot` no longer orphans it.
    cfg.agents.insert(
        "bot2".to_string(),
        AliasedAgentConfig {
            enabled: true,
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    let report = plan_delete(&cfg, &AliasKind::Agent, "bot");
    assert!(
        report.allowed,
        "co-owned channel must not block: {:?}",
        report.blockers
    );

    // A DISABLED owner owns nothing, so its delete doesn't block either.
    let mut cfg = empty_config();
    cfg.agents.insert(
        "off".to_string(),
        AliasedAgentConfig {
            enabled: false,
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    assert!(plan_delete(&cfg, &AliasKind::Agent, "off").allowed);
}

// ── delete_with_cascade (model providers) ───────────────────────────────

fn cfg_with_provider(family: &str, alias: &str) -> Config {
    let mut c = empty_config();
    c.providers
        .models
        .ensure(family, alias)
        .expect("ensure creates the entry");
    c
}

#[test]
fn cascade_refuses_when_model_provider_is_hard_ref() {
    let mut cfg = cfg_with_provider("anthropic", "default");
    cfg.agents.insert(
        "researcher".to_string(),
        AliasedAgentConfig {
            model_provider: "anthropic.default".into(),
            ..Default::default()
        },
    );
    let kind = provider_kind("anthropic");
    let err =
        delete_with_cascade(&mut cfg, &kind, "default", CascadePolicy::RefuseOnHard).unwrap_err();
    match err {
        CascadeError::Refused(report) => assert_eq!(report.blockers.len(), 1),
        other => panic!("expected Refused, got {other:?}"),
    }
    // No mutation on refuse.
    assert!(cfg.providers.models.find("anthropic", "default").is_some());
    assert_eq!(
        cfg.agents["researcher"].model_provider.as_str(),
        "anthropic.default"
    );
}

#[test]
fn cascade_scrubs_soft_refs_and_removes_entry() {
    let mut cfg = cfg_with_provider("anthropic", "default");
    // Another provider whose fallback points at the target.
    cfg.providers
        .models
        .ensure("openai", "main")
        .unwrap()
        .fallback = vec!["anthropic.default".into()];
    cfg.agents.insert(
        "triage".to_string(),
        AliasedAgentConfig {
            classifier_provider: "anthropic.default".into(),
            ..Default::default()
        },
    );
    cfg.model_routes.push(ModelRouteConfig {
        hint: "deep".to_string(),
        model_provider: "anthropic.default".to_string(),
        model: "claude".to_string(),
        api_key: None,
    });
    cfg.embedding_routes.push(EmbeddingRouteConfig {
        hint: "sem".to_string(),
        model_provider: "anthropic.default".to_string(),
        model: "emb".to_string(),
        dimensions: None,
        api_key: None,
    });

    let kind = provider_kind("anthropic");
    let report = delete_with_cascade(&mut cfg, &kind, "default", CascadePolicy::RefuseOnHard)
        .expect("soft-only delete succeeds");
    assert_eq!(
        report.applied.len(),
        4,
        "classifier + fallback + model_route + embedding_route"
    );
    assert_eq!(
        report.deleted_entry.as_deref(),
        Some("providers.models.anthropic.default")
    );
    assert!(cfg.providers.models.find("anthropic", "default").is_none());
    assert!(cfg.agents["triage"].classifier_provider.is_empty());
    assert!(
        cfg.providers
            .models
            .find("openai", "main")
            .unwrap()
            .fallback
            .is_empty()
    );
    assert!(cfg.model_routes.is_empty());
    assert!(cfg.embedding_routes.is_empty());
    assert!(find_all_references(&cfg, &kind, "default").is_empty());
}

#[test]
fn cascade_scrubs_whitespace_padded_refs() {
    // scrub must trim like find/validate, else a padded ref find() flags is
    // left behind and the post-condition fails.
    let mut cfg = cfg_with_provider("anthropic", "default");
    cfg.agents.insert(
        "triage".to_string(),
        AliasedAgentConfig {
            classifier_provider: "  anthropic.default  ".into(),
            ..Default::default()
        },
    );
    cfg.model_routes.push(ModelRouteConfig {
        hint: "deep".to_string(),
        model_provider: " anthropic.default ".to_string(),
        model: "claude".to_string(),
        api_key: None,
    });
    let kind = provider_kind("anthropic");
    let report = delete_with_cascade(&mut cfg, &kind, "default", CascadePolicy::RefuseOnHard)
        .expect("padded soft refs scrubbed, post-condition passes");
    assert_eq!(report.applied.len(), 2);
    assert!(cfg.agents["triage"].classifier_provider.is_empty());
    assert!(cfg.model_routes.is_empty());
}

#[test]
fn cascade_scrubs_all_matching_fallback_entries() {
    let mut cfg = cfg_with_provider("anthropic", "default");
    // openai.main lists the target twice in fallback (plus an unrelated one);
    // retain must drop BOTH matches and keep the unrelated entry.
    cfg.providers
        .models
        .ensure("openai", "main")
        .unwrap()
        .fallback = vec![
        "anthropic.default".into(),
        "anthropic.fast".into(),
        "anthropic.default".into(),
    ];
    let kind = provider_kind("anthropic");
    let report = delete_with_cascade(&mut cfg, &kind, "default", CascadePolicy::RefuseOnHard)
        .expect("soft-only delete succeeds");
    assert_eq!(
        report.applied.len(),
        2,
        "both matching fallback entries reported"
    );
    let fallback = &cfg
        .providers
        .models
        .find("openai", "main")
        .unwrap()
        .fallback;
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].as_str(), "anthropic.fast");
}

#[test]
fn cascade_dry_run_mutates_nothing() {
    let mut cfg = cfg_with_provider("anthropic", "default");
    cfg.agents.insert(
        "triage".to_string(),
        AliasedAgentConfig {
            classifier_provider: "anthropic.default".into(),
            ..Default::default()
        },
    );
    let kind = provider_kind("anthropic");
    let report = delete_with_cascade(&mut cfg, &kind, "default", CascadePolicy::DryRun).unwrap();
    assert!(report.deleted_entry.is_none());
    assert!(report.applied.is_empty());
    assert_eq!(report.plan.scrubs.len(), 1);
    assert!(cfg.providers.models.find("anthropic", "default").is_some());
    assert_eq!(
        cfg.agents["triage"].classifier_provider.as_str(),
        "anthropic.default"
    );
}

#[test]
fn cascade_not_found_for_missing_provider() {
    let mut cfg = empty_config();
    let err = delete_with_cascade(
        &mut cfg,
        &provider_kind("anthropic"),
        "ghost",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    assert!(matches!(err, CascadeError::NotFound(_)));
}

#[test]
fn cascade_removes_unreferenced_provider() {
    let mut cfg = cfg_with_provider("anthropic", "spare");
    let report = delete_with_cascade(
        &mut cfg,
        &provider_kind("anthropic"),
        "spare",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap();
    assert!(report.applied.is_empty());
    assert_eq!(
        report.deleted_entry.as_deref(),
        Some("providers.models.anthropic.spare")
    );
    assert!(cfg.providers.models.find("anthropic", "spare").is_none());
}

#[test]
fn cascade_not_implemented_for_other_kinds() {
    // Only TTS/transcription providers remain unimplemented now (model
    // providers, agents, and channels are all wired).
    let mut cfg = empty_config();
    for category in [ProviderCategory::Tts, ProviderCategory::Transcription] {
        let kind = AliasKind::Provider {
            category,
            family: "x".to_string(),
        };
        assert!(matches!(
            delete_with_cascade(&mut cfg, &kind, "x", CascadePolicy::RefuseOnHard),
            Err(CascadeError::NotImplemented(_))
        ));
    }
}

// ── delete_with_cascade (agents) ────────────────────────────────────────

#[test]
fn cascade_agent_refuses_when_heartbeat_enabled() {
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    cfg.heartbeat.enabled = true;
    cfg.heartbeat.agent = "bot".to_string();
    let err = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "bot",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    match err {
        CascadeError::Refused(report) => {
            assert_eq!(report.blockers.len(), 1);
            assert_eq!(report.blockers[0].path, "heartbeat.agent");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(cfg.agents.contains_key("bot"));
    assert_eq!(cfg.heartbeat.agent.as_str(), "bot");
}

#[test]
fn cascade_agent_refuses_when_solely_owned_channel() {
    // The agent arm of `delete_with_cascade` must also refuse on a sole-owned
    // channel — the second HARD agent ref besides an enabled `heartbeat.agent`
    // — before any mutation, locking the mutating path against future
    // scrub/collect drift (the plan-only case is `agent_delete_blocks_on_solely_owned_channel`).
    let mut cfg = empty_config();
    cfg.agents.insert(
        "bot".to_string(),
        AliasedAgentConfig {
            enabled: true,
            channels: vec!["discord.main".into()], // bot is the sole enabled owner
            ..Default::default()
        },
    );
    let err = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "bot",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    match err {
        CascadeError::Refused(report) => {
            assert!(
                report
                    .blockers
                    .iter()
                    .any(|b| b.path == "agents.bot.channels[0]"),
                "{:?}",
                report.blockers
            );
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    // Refuse-before-mutate: the agent and its channel ownership survive intact.
    assert!(cfg.agents.contains_key("bot"));
    assert_eq!(cfg.agents["bot"].channels, vec!["discord.main".to_string()]);
}

#[test]
fn cascade_agent_scrubs_all_soft_refs_and_removes() {
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    cfg.heartbeat.enabled = false; // disabled → heartbeat.agent is a SOFT ref
    cfg.heartbeat.agent = "bot".to_string();
    cfg.acp.default_agent = Some("bot".to_string());
    let mut lead = AliasedAgentConfig {
        ..Default::default()
    };
    lead.workspace
        .access
        .insert(AgentAlias::new("bot"), AccessMode::Read);
    lead.workspace.read_memory_from.push(AgentAlias::new("bot"));
    cfg.agents.insert("lead".to_string(), lead);
    cfg.peer_groups.insert(
        "crew".to_string(),
        PeerGroupConfig {
            agents: vec![AgentAlias::new("bot")],
            ..Default::default()
        },
    );

    let report = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "bot",
        CascadePolicy::RefuseOnHard,
    )
    .expect("soft-only agent delete succeeds");
    assert_eq!(report.applied.len(), 5);
    assert_eq!(report.deleted_entry.as_deref(), Some("agents.bot"));
    assert!(!cfg.agents.contains_key("bot"));
    assert!(cfg.heartbeat.agent.is_empty());
    assert!(cfg.acp.default_agent.is_none());
    assert!(cfg.agents["lead"].workspace.access.is_empty());
    assert!(cfg.agents["lead"].workspace.read_memory_from.is_empty());
    assert!(cfg.peer_groups["crew"].agents.is_empty());
    assert!(find_all_references(&cfg, &AliasKind::Agent, "bot").is_empty());
}

#[test]
fn cascade_agent_scrub_trim_split_mirrors_find() {
    // Trimmed sites (heartbeat/acp) scrub a padded ref; raw sites
    // (read_memory_from) do not — exactly as find/validate.
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    cfg.heartbeat.enabled = false;
    cfg.heartbeat.agent = "  bot  ".to_string();
    cfg.acp.default_agent = Some(" bot ".to_string());
    let mut lead = AliasedAgentConfig {
        ..Default::default()
    };
    lead.workspace
        .read_memory_from
        .push(AgentAlias::new(" bot ")); // raw, must remain
    cfg.agents.insert("lead".to_string(), lead);

    let report = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "bot",
        CascadePolicy::RefuseOnHard,
    )
    .expect("padded trimmed refs scrubbed, post-condition passes");
    assert_eq!(report.applied.len(), 2, "heartbeat + acp (trimmed)");
    assert!(cfg.heartbeat.agent.is_empty());
    assert!(cfg.acp.default_agent.is_none());
    // raw read_memory_from did not match " bot " != "bot" → untouched.
    assert_eq!(cfg.agents["lead"].workspace.read_memory_from.len(), 1);
}

#[test]
fn cascade_agent_dry_run_mutates_nothing() {
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    cfg.acp.default_agent = Some("bot".to_string());
    let report =
        delete_with_cascade(&mut cfg, &AliasKind::Agent, "bot", CascadePolicy::DryRun).unwrap();
    assert!(report.deleted_entry.is_none());
    assert_eq!(report.plan.scrubs.len(), 1);
    assert!(cfg.agents.contains_key("bot"));
    assert_eq!(cfg.acp.default_agent.as_deref(), Some("bot"));
}

#[test]
fn cascade_agent_not_found() {
    let mut cfg = empty_config();
    let err = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "ghost",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    assert!(matches!(err, CascadeError::NotFound(_)));
}

#[test]
fn cascade_agent_self_reference_is_scrubbed() {
    // An agent that names ITSELF in read_memory_from: deleting it
    // must succeed (the scrub loop processes the to-be-deleted agent and
    // strips the self-refs before the entry is removed; the post-condition
    // then confirms nothing dangles).
    let mut cfg = empty_config();
    let mut bot = AliasedAgentConfig {
        ..Default::default()
    };
    bot.workspace.read_memory_from.push(AgentAlias::new("bot"));
    cfg.agents.insert("bot".to_string(), bot);

    let report = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "bot",
        CascadePolicy::RefuseOnHard,
    )
    .expect("self-referencing agent deletes cleanly");
    assert_eq!(report.deleted_entry.as_deref(), Some("agents.bot"));
    assert!(!cfg.agents.contains_key("bot"));
    assert!(find_all_references(&cfg, &AliasKind::Agent, "bot").is_empty());
}

// ── delete_with_cascade (channels) ──────────────────────────────────────

fn channel_kind() -> AliasKind {
    AliasKind::Channel {
        channel_type: "discord".to_string(),
    }
}

fn has_channel(cfg: &Config, alias: &str) -> bool {
    cfg.get_map_keys("channels.discord")
        .unwrap_or_default()
        .iter()
        .any(|k| k == alias)
}

#[test]
fn cascade_channel_scrubs_soft_refs_and_removes_entry() {
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    cfg.agents.insert(
        "ops".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    cfg.escalation
        .alert_channels
        .push("discord.main".to_string());

    let report = delete_with_cascade(
        &mut cfg,
        &channel_kind(),
        "main",
        CascadePolicy::RefuseOnHard,
    )
    .expect("soft-only channel delete succeeds");
    assert_eq!(report.applied.len(), 2, "agent channel + alert_channel");
    assert_eq!(
        report.deleted_entry.as_deref(),
        Some("channels.discord.main")
    );
    assert!(!has_channel(&cfg, "main"));
    assert!(cfg.agents["ops"].channels.is_empty());
    assert!(cfg.escalation.alert_channels.is_empty());
    assert!(find_all_references(&cfg, &channel_kind(), "main").is_empty());
}

#[test]
fn cascade_channel_refuses_on_hard_peer_group_ref() {
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    cfg.peer_groups.insert(
        "crew".to_string(),
        PeerGroupConfig {
            channel: "discord.main".into(),
            ..Default::default()
        },
    );
    let err = delete_with_cascade(
        &mut cfg,
        &channel_kind(),
        "main",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    match err {
        CascadeError::Refused(report) => {
            assert_eq!(report.blockers[0].path, "peer_groups.crew.channel");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(has_channel(&cfg, "main"), "no mutation on refuse");
}

#[test]
fn cascade_channel_dry_run_mutates_nothing() {
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    cfg.agents.insert(
        "ops".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    let report =
        delete_with_cascade(&mut cfg, &channel_kind(), "main", CascadePolicy::DryRun).unwrap();
    assert!(report.deleted_entry.is_none());
    assert_eq!(report.plan.scrubs.len(), 1);
    assert!(has_channel(&cfg, "main"));
    assert_eq!(cfg.agents["ops"].channels.len(), 1);
}

#[test]
fn cascade_channel_not_found() {
    let mut cfg = empty_config();
    let err = delete_with_cascade(
        &mut cfg,
        &channel_kind(),
        "ghost",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    assert!(matches!(err, CascadeError::NotFound(_)));
}

#[test]
fn cascade_channel_refuses_orphaning_bare_group_member() {
    // BARE-type group ("discord", not "discord.main"). validate
    // (schema.rs:17461-17478) requires each member to keep some `discord.*`
    // channel. `ops`'s only discord channel is the one being deleted, so the
    // delete must REFUSE — scrubbing it would yield a config validate() rejects.
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    cfg.agents.insert(
        "ops".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    let mut group = PeerGroupConfig {
        channel: "discord".into(), // bare type
        ..Default::default()
    };
    group.agents.push(AgentAlias::new("ops"));
    cfg.peer_groups.insert("crew".to_string(), group);

    let err = delete_with_cascade(
        &mut cfg,
        &channel_kind(),
        "main",
        CascadePolicy::RefuseOnHard,
    )
    .unwrap_err();
    match err {
        CascadeError::Refused(report) => {
            assert!(
                report
                    .blockers
                    .iter()
                    .any(|b| b.path == "peer_groups.crew.agents[0]"),
                "bare-group member orphan must be a hard blocker, got {:?}",
                report.blockers
            );
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(has_channel(&cfg, "main"), "no mutation on refuse");
    assert_eq!(
        cfg.agents["ops"].channels.len(),
        1,
        "member channel not scrubbed on refuse"
    );
}

#[test]
fn cascade_channel_proceeds_when_bare_group_member_keeps_another() {
    // Same bare-type group, but `ops` also has `discord.backup`. Deleting
    // `discord.main` leaves it with a surviving `discord.*`, so membership
    // stays valid and the delete proceeds (scrubbing only the main ref).
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    cfg.create_map_key("channels.discord", "backup").unwrap();
    cfg.agents.insert(
        "ops".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into(), "discord.backup".into()],
            ..Default::default()
        },
    );
    let mut group = PeerGroupConfig {
        channel: "discord".into(), // bare type
        ..Default::default()
    };
    group.agents.push(AgentAlias::new("ops"));
    cfg.peer_groups.insert("crew".to_string(), group);

    let report = delete_with_cascade(
        &mut cfg,
        &channel_kind(),
        "main",
        CascadePolicy::RefuseOnHard,
    )
    .expect("delete proceeds when a sibling channel keeps membership valid");
    assert_eq!(
        report.deleted_entry.as_deref(),
        Some("channels.discord.main")
    );
    assert!(!has_channel(&cfg, "main"));
    let remaining: Vec<&str> = cfg.agents["ops"]
        .channels
        .iter()
        .map(|c| c.as_str())
        .collect();
    assert_eq!(
        remaining,
        vec!["discord.backup"],
        "only the deleted channel is scrubbed; backup survives"
    );
}

// ── rename_with_cascade─────────────────────────────────────────

#[test]
fn rename_agent_rewrites_every_ref_kind() {
    let mut cfg = empty_config();
    cfg.heartbeat.enabled = true;
    cfg.heartbeat.agent = "bot".to_string(); // HARD ref — rename rewrites it
    cfg.acp.default_agent = Some("bot".to_string());
    let mut bot = AliasedAgentConfig {
        ..Default::default()
    };
    bot.workspace
        .access
        .insert(AgentAlias::new("bot"), AccessMode::Read);
    cfg.agents.insert("bot".to_string(), bot);
    // A referrer agent pointing at bot every which way.
    let mut lead = AliasedAgentConfig {
        ..Default::default()
    };
    lead.workspace
        .access
        .insert(AgentAlias::new("bot"), AccessMode::Read);
    lead.workspace.read_memory_from.push(AgentAlias::new("bot"));
    cfg.agents.insert("lead".to_string(), lead);
    let mut group = PeerGroupConfig::default();
    group.agents.push(AgentAlias::new("bot"));
    cfg.peer_groups.insert("crew".to_string(), group);

    let report = rename_with_cascade(&mut cfg, &AliasKind::Agent, "bot", "bot2")
        .expect("agent rename succeeds");
    assert_eq!(report.new_alias, "bot2");
    // entry moved
    assert!(!cfg.agents.contains_key("bot"));
    assert!(cfg.agents.contains_key("bot2"));
    // every ref now names bot2
    assert_eq!(cfg.heartbeat.agent, "bot2");
    assert_eq!(cfg.acp.default_agent.as_deref(), Some("bot2"));
    assert!(
        cfg.agents["bot2"]
            .workspace
            .access
            .contains_key(&AgentAlias::new("bot2"))
    );
    assert!(
        cfg.agents["lead"]
            .workspace
            .access
            .contains_key(&AgentAlias::new("bot2"))
    );
    assert_eq!(
        cfg.agents["lead"].workspace.read_memory_from,
        vec![AgentAlias::new("bot2")]
    );
    assert_eq!(
        cfg.peer_groups["crew"].agents,
        vec![AgentAlias::new("bot2")]
    );
    // post-condition: nothing references the old alias
    assert!(find_all_references(&cfg, &AliasKind::Agent, "bot").is_empty());
    // dirty paths cover every touched entry/section + the entry-key swap, so
    // the surface persists exactly what changed (and nothing stays stale).
    for expected in [
        "heartbeat.agent",
        "acp.default_agent",
        "agents.bot", // old entry removed on disk
        "agents.bot2",
        "agents.lead",
        "peer_groups.crew",
    ] {
        assert!(
            report.dirty_paths.iter().any(|p| p == expected),
            "missing dirty path {expected:?} in {:?}",
            report.dirty_paths
        );
    }
    // sorted + deduplicated
    let mut sorted = report.dirty_paths.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, report.dirty_paths);
}

#[test]
fn rename_agent_not_found() {
    let mut cfg = empty_config();
    let err = rename_with_cascade(&mut cfg, &AliasKind::Agent, "ghost", "specter").unwrap_err();
    assert!(matches!(err, RenameError::NotFound(_)));
}

#[test]
fn rename_agent_collision_is_invalid() {
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    cfg.agents
        .insert("other".to_string(), AliasedAgentConfig::default());
    let err = rename_with_cascade(&mut cfg, &AliasKind::Agent, "bot", "other").unwrap_err();
    assert!(matches!(err, RenameError::InvalidName(_)));
    // no mutation: both entries still present
    assert!(cfg.agents.contains_key("bot"));
    assert!(cfg.agents.contains_key("other"));
}

#[test]
fn rename_agent_noop_is_invalid() {
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    let err = rename_with_cascade(&mut cfg, &AliasKind::Agent, "bot", "bot").unwrap_err();
    assert!(matches!(err, RenameError::InvalidName(_)));
}

#[test]
fn rename_default_agent_is_reserved_both_directions() {
    let mut cfg = empty_config();
    cfg.agents
        .insert("default".to_string(), AliasedAgentConfig::default());
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    // can't rename the default agent away
    let from = rename_with_cascade(&mut cfg, &AliasKind::Agent, "default", "primary").unwrap_err();
    assert!(matches!(from, RenameError::Reserved(_)));
    // can't rename another agent onto `default`
    let onto = rename_with_cascade(&mut cfg, &AliasKind::Agent, "bot", "default").unwrap_err();
    assert!(matches!(onto, RenameError::Reserved(_)));
    // nothing mutated
    assert!(cfg.agents.contains_key("default"));
    assert!(cfg.agents.contains_key("bot"));
}

#[test]
fn is_reserved_agent_alias_flags_only_default() {
    // The shared create guard uses this to refuse `default` symmetrically
    // with the rename guard (so no surface can author an undeletable agent).
    assert!(is_reserved_agent_alias("default"));
    assert!(is_reserved_agent_alias("  default  ")); // trims before comparing
    assert!(!is_reserved_agent_alias("default2"));
    assert!(!is_reserved_agent_alias("cronos"));
    assert!(!is_reserved_agent_alias(""));
}

#[test]
fn create_map_key_checked_refuses_reserved_default_agent() {
    let mut cfg = empty_config();
    // The reserved `default` agent cannot be created, and nothing is
    // inserted -- the create analogue of rename_default_agent_is_reserved.
    let err = create_map_key_checked(&mut cfg, "agents", "default").unwrap_err();
    assert!(matches!(err, CreateError::Reserved(_)));
    assert!(!cfg.agents.contains_key("default"));
    // A whitespace-padded variant is refused the same way.
    assert!(matches!(
        create_map_key_checked(&mut cfg, "agents", "  default  ").unwrap_err(),
        CreateError::Reserved(_)
    ));
    // A non-reserved agent alias is created and persisted in memory.
    assert!(create_map_key_checked(&mut cfg, "agents", "scout").unwrap());
    assert!(cfg.agents.contains_key("scout"));
    // Agent-scoped only: `default` is a free key for non-agent kinds, so the
    // guard delegates rather than refusing it as reserved.
    assert!(create_map_key_checked(&mut cfg, "providers.models.anthropic", "default").unwrap());
    // An unknown section surfaces as Invalid, not Reserved.
    assert!(matches!(
        create_map_key_checked(&mut cfg, "not.a.real.section", "x").unwrap_err(),
        CreateError::Invalid(_)
    ));
}

#[test]
fn ensure_map_key_for_path_refuses_reserved_default_agent() {
    let mut cfg = empty_config();
    // A set-prop on a nonexistent `agents.default` must NOT auto-vivify the
    // reserved runtime-fallback agent, and signals the refusal (true) so the
    // set-prop surface returns a reserved error (PUT /prop, PATCH, RPC set).
    assert!(cfg.ensure_map_key_for_path("agents.default.enabled"));
    assert!(!cfg.agents.contains_key("default"));
    // A non-reserved agent IS vivified (not refused), as normal set-prop-on-new.
    assert!(!cfg.ensure_map_key_for_path("agents.scout.enabled"));
    assert!(cfg.agents.contains_key("scout"));
    // An already-present `default` (e.g. migration-synthesized) is left intact
    // and still configurable: the existence check returns false (not refused).
    cfg.agents
        .insert("default".to_string(), AliasedAgentConfig::default());
    assert!(!cfg.ensure_map_key_for_path("agents.default.model"));
    assert!(cfg.agents.contains_key("default"));
}

#[test]
fn rename_rejects_deleted_marker_target() {
    // `_deleted` is blocked as a new alias by validate_alias_key (leading
    // underscore) — surfaced as InvalidName via rename_map_key.
    let mut cfg = empty_config();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    let err = rename_with_cascade(&mut cfg, &AliasKind::Agent, "bot", "_deleted").unwrap_err();
    assert!(matches!(err, RenameError::InvalidName(_)));
    assert!(cfg.agents.contains_key("bot"));
}

#[test]
fn rename_model_provider_rewrites_dotted_refs() {
    let mut cfg = cfg_with_provider("anthropic", "default");
    cfg.agents.insert(
        "researcher".to_string(),
        AliasedAgentConfig {
            model_provider: "anthropic.default".into(), // HARD — rewritten, not refused
            classifier_provider: "anthropic.default".into(),
            ..Default::default()
        },
    );
    // another provider whose fallback names the target
    cfg.providers
        .models
        .ensure("openai", "fast")
        .expect("ensure");
    for (_t, al, p) in cfg.providers.models.iter_entries_mut() {
        if al == "fast" {
            p.fallback.push("anthropic.default".into());
        }
    }
    cfg.model_routes.push(ModelRouteConfig {
        hint: "deep".to_string(),
        model_provider: "anthropic.default".to_string(),
        model: "claude".to_string(),
        api_key: None,
    });

    let kind = provider_kind("anthropic");
    let report =
        rename_with_cascade(&mut cfg, &kind, "default", "prod").expect("provider rename succeeds");
    assert_eq!(report.new_alias, "prod");
    assert!(cfg.providers.models.find("anthropic", "default").is_none());
    assert!(cfg.providers.models.find("anthropic", "prod").is_some());
    assert_eq!(
        cfg.agents["researcher"].model_provider.as_str(),
        "anthropic.prod"
    );
    assert_eq!(
        cfg.agents["researcher"].classifier_provider.as_str(),
        "anthropic.prod"
    );
    assert_eq!(cfg.model_routes[0].model_provider, "anthropic.prod");
    let fast_fallback: Vec<String> = cfg
        .providers
        .models
        .iter_entries()
        .filter(|(_, al, _)| *al == "fast")
        .flat_map(|(_, _, p)| p.fallback.iter().map(|f| f.as_str().to_string()))
        .collect();
    assert_eq!(
        fast_fallback,
        vec!["anthropic.prod".to_string()],
        "fallback rewritten"
    );
    assert!(find_all_references(&cfg, &kind, "default").is_empty());
}

#[test]
fn rename_channel_rewrites_refs_and_preserves_bare_group_membership() {
    let mut cfg = empty_config();
    cfg.create_map_key("channels.discord", "main").unwrap();
    // member of a BARE-type group whose only discord channel is the target:
    // delete would REFUSE (orphan), but rename keeps membership valid.
    cfg.agents.insert(
        "ops".to_string(),
        AliasedAgentConfig {
            channels: vec!["discord.main".into()],
            ..Default::default()
        },
    );
    let mut group = PeerGroupConfig {
        channel: "discord".into(), // bare type
        ..Default::default()
    };
    group.agents.push(AgentAlias::new("ops"));
    cfg.peer_groups.insert("crew".to_string(), group);
    // also a dotted peer-group channel + an alert channel
    cfg.peer_groups.insert(
        "ops_team".to_string(),
        PeerGroupConfig {
            channel: "discord.main".into(), // dotted HARD ref — rewritten
            ..Default::default()
        },
    );
    cfg.escalation
        .alert_channels
        .push("discord.main".to_string());

    let kind = channel_kind();
    let report = rename_with_cascade(&mut cfg, &kind, "main", "primary")
        .expect("channel rename succeeds (no orphan, unlike delete)");
    assert_eq!(report.new_alias, "primary");
    assert!(!has_channel(&cfg, "main"));
    assert!(has_channel(&cfg, "primary"));
    assert_eq!(
        cfg.agents["ops"]
            .channels
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>(),
        vec!["discord.primary"],
        "member still has a discord.* channel — membership preserved"
    );
    assert_eq!(
        cfg.peer_groups["ops_team"].channel.as_str(),
        "discord.primary"
    );
    assert_eq!(
        cfg.escalation.alert_channels,
        vec!["discord.primary".to_string()]
    );
    assert!(find_all_references(&cfg, &kind, "main").is_empty());
}

#[test]
fn rename_tts_provider_rewrites_scalar() {
    let mut cfg = empty_config();
    cfg.create_map_key("providers.tts.elevenlabs", "default")
        .expect("create tts entry");
    cfg.agents.insert(
        "voice".to_string(),
        AliasedAgentConfig {
            tts_provider: "elevenlabs.default".into(),
            ..Default::default()
        },
    );
    let kind = AliasKind::Provider {
        category: ProviderCategory::Tts,
        family: "elevenlabs".to_string(),
    };
    let report = rename_with_cascade(&mut cfg, &kind, "default", "studio")
        .expect("tts provider rename succeeds");
    assert!(report.dirty_paths.iter().any(|p| p == "agents.voice"));
    assert!(
        report
            .dirty_paths
            .iter()
            .any(|p| p == "providers.tts.elevenlabs.studio")
    );
    assert_eq!(
        cfg.agents["voice"].tts_provider.as_str(),
        "elevenlabs.studio"
    );
    assert!(find_all_references(&cfg, &kind, "default").is_empty());
}

#[test]
fn dirty_entry_for_truncates_ref_paths_to_persistable_entries() {
    // agent / peer-group referrer sites → the entry root (whole subtree).
    assert_eq!(
        dirty_entry_for("agents.lead.workspace.read_memory_from.bot"),
        "agents.lead"
    );
    assert_eq!(
        dirty_entry_for("agents.lead.workspace.access.bot"),
        "agents.lead"
    );
    assert_eq!(
        dirty_entry_for("peer_groups.crew.agents[1]"),
        "peer_groups.crew"
    );
    // scalars / whole-vector fields → the field/section, index stripped.
    assert_eq!(dirty_entry_for("heartbeat.agent"), "heartbeat.agent");
    assert_eq!(dirty_entry_for("acp.default_agent"), "acp.default_agent");
    assert_eq!(
        dirty_entry_for("escalation.alert_channels[3]"),
        "escalation.alert_channels"
    );
    assert_eq!(
        dirty_entry_for("model_routes[0].model_provider"),
        "model_routes"
    );
    // provider entry → the 4-segment entry path.
    assert_eq!(
        dirty_entry_for("providers.models.anthropic.default.fallback[0]"),
        "providers.models.anthropic.default"
    );
}

#[test]
fn cascade_report_dirty_paths_covers_scrubs_and_deleted_entry() {
    // A delete that scrubbed two referrers in different entries + removed the
    // entry must report all three dirty paths (deduped, sorted).
    let mut cfg = empty_config();
    cfg.heartbeat.enabled = false;
    cfg.heartbeat.agent = "bot".to_string();
    cfg.agents
        .insert("bot".to_string(), AliasedAgentConfig::default());
    let mut lead = AliasedAgentConfig {
        ..Default::default()
    };
    lead.workspace
        .access
        .insert(AgentAlias::new("bot"), AccessMode::Read);
    cfg.agents.insert("lead".to_string(), lead);
    let report = delete_with_cascade(
        &mut cfg,
        &AliasKind::Agent,
        "bot",
        CascadePolicy::RefuseOnHard,
    )
    .expect("delete succeeds");
    let dirty = report.dirty_paths();
    assert!(
        dirty.contains(&"agents.bot".to_string()),
        "removed entry: {dirty:?}"
    );
    assert!(
        dirty.contains(&"agents.lead".to_string()),
        "scrubbed referrer: {dirty:?}"
    );
    assert!(
        dirty.contains(&"heartbeat.agent".to_string()),
        "cleared heartbeat: {dirty:?}"
    );
}

#[test]
fn bundle_refs_find_scrub_rewrite() {
    let mut cfg = empty_config();
    cfg.agents.insert(
        "a".to_string(),
        AliasedAgentConfig {
            skill_bundles: vec!["util".to_string(), "web".to_string()],
            ..Default::default()
        },
    );
    cfg.agents.insert(
        "b".to_string(),
        AliasedAgentConfig {
            skill_bundles: vec![" util ".to_string()], // padded — validate trims
            ..Default::default()
        },
    );

    // find (trim-matched across both agents)
    let sites = find_bundle_refs(&cfg, "util");
    assert_eq!(sites.len(), 2, "{sites:?}");
    assert!(sites.iter().all(|s| s.strength == RefStrength::Soft));

    // rewrite util -> tools
    let dirty = rewrite_bundle_refs(&mut cfg, "util", "tools");
    assert_eq!(dirty.len(), 2);
    assert_eq!(
        cfg.agents["a"].skill_bundles,
        vec!["tools".to_string(), "web".to_string()]
    );
    assert_eq!(cfg.agents["b"].skill_bundles, vec!["tools".to_string()]);
    assert!(find_bundle_refs(&cfg, "util").is_empty());

    // scrub tools from all agents
    let dirty = scrub_bundle_refs(&mut cfg, "tools");
    assert_eq!(dirty.len(), 2);
    assert_eq!(cfg.agents["a"].skill_bundles, vec!["web".to_string()]);
    assert!(cfg.agents["b"].skill_bundles.is_empty());
    assert!(find_bundle_refs(&cfg, "tools").is_empty());
}

#[test]
fn advisor_model_ref_is_scrubbed_on_delete_and_rewritten_on_rename() {
    use crate::advisor::AdvisorTarget;

    let mut cfg = cfg_with_provider("anthropic", "default");
    cfg.agents.insert(
        "fast".to_string(),
        AliasedAgentConfig {
            advisor: Some(AdvisorTarget::Model("anthropic.default".into())),
            ..Default::default()
        },
    );
    let kind = provider_kind("anthropic");

    let sites = find_all_references(&cfg, &kind, "default");
    assert!(
        sites
            .iter()
            .any(|s| s.path == "agents.fast.advisor" && s.raw_value == "model:anthropic.default"),
        "advisor must be listed as a soft reference: {sites:?}"
    );

    rename_with_cascade(&mut cfg, &kind, "default", "prod").expect("rename succeeds");
    assert_eq!(
        cfg.agents["fast"].advisor,
        Some(AdvisorTarget::Model("anthropic.prod".into()))
    );

    delete_with_cascade(&mut cfg, &kind, "prod", CascadePolicy::RefuseOnHard)
        .expect("advisor is a soft ref; delete succeeds");
    assert!(cfg.agents["fast"].advisor.is_none());
}
