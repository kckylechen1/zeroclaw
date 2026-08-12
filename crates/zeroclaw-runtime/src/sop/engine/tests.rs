//! Unit tests for [`SopEngine`].
//!
//! Extracted from `engine/mod.rs` so production orchestrator code and the
//! large behavioral suite can evolve independently.

use super::super::store::ProposalKind;
use super::super::time::parse_iso8601_secs;
use super::*;
use crate::calendar::CALENDAR_NO_SHOW_TOPIC;
use crate::sop::approval::{ApprovalDecision, ApprovalPrincipal, ResolveOutcome};
use crate::sop::step_contract::StepFailure;
use crate::sop::types::{SopExecutionMode, StepSchema};

/// Clear a WaitingApproval gate through the production out-of-band chokepoint
/// (a CLI principal), returning the resumed action. Mirrors what a real
/// `zeroclaw sop approve` does, replacing the old `approve_step` agent path.
fn approve_gate_cli(engine: &mut SopEngine, run_id: &str) -> SopRunAction {
    match engine
        .resolve_gate(
            run_id,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap()
    {
        ResolveOutcome::Resumed(action) => *action,
        other => panic!("expected Resumed, got {other:?}"),
    }
}

fn manual_event() -> SopEvent {
    SopEvent {
        source: SopTriggerSource::Manual,
        topic: None,
        payload: None,
        timestamp: now_iso8601(),
    }
}

fn mqtt_event(topic: &str, payload: &str) -> SopEvent {
    SopEvent {
        source: SopTriggerSource::Mqtt,
        topic: Some(topic.into()),
        payload: Some(payload.into()),
        timestamp: now_iso8601(),
    }
}

fn test_sop(name: &str, mode: SopExecutionMode, priority: SopPriority) -> Sop {
    Sop {
        name: name.into(),
        description: format!("Test SOP: {name}"),
        version: "1.0.0".into(),
        priority,
        execution_mode: mode,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Step one".into(),
                body: "Do step one".into(),
                suggested_tools: vec!["shell".into()],
                requires_confirmation: false,
                kind: SopStepKind::default(),
                schema: None,
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Step two".into(),
                body: "Do step two".into(),
                suggested_tools: vec![],
                requires_confirmation: false,
                kind: SopStepKind::default(),
                schema: None,
                ..SopStep::default()
            },
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: false,
        admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
        agent: None,
    }
}

fn engine_with_sops(sops: Vec<Sop>) -> SopEngine {
    engine_with_config_sops(SopConfig::default(), sops)
}

fn engine_with_config_sops(config: SopConfig, sops: Vec<Sop>) -> SopEngine {
    let mut engine = SopEngine::new(config);
    engine.sops = sops;
    engine
}

fn required_object_schema(key: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": [key]
    })
}

/// Extract run_id from any SopRunAction variant.
fn extract_run_id(action: &SopRunAction) -> &str {
    match action {
        SopRunAction::ExecuteStep { run_id, .. }
        | SopRunAction::WaitApproval { run_id, .. }
        | SopRunAction::DeterministicStep { run_id, .. }
        | SopRunAction::CheckpointWait { run_id, .. }
        | SopRunAction::Pending { run_id, .. }
        | SopRunAction::Completed { run_id, .. }
        | SopRunAction::Failed { run_id, .. } => run_id,
    }
}

/// Get the first active run_id from the engine (for tests with a single run).
#[allow(dead_code)]
fn first_active_run_id(engine: &SopEngine) -> String {
    engine
        .active_runs()
        .keys()
        .next()
        .expect("expected at least one active run")
        .clone()
}

// ── Trigger matching ────────────────────────────────

#[test]
fn match_manual_trigger() {
    let engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let matches = engine.match_trigger(&manual_event());
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "s1");
}

#[test]
fn no_match_for_wrong_source() {
    let engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let event = mqtt_event("sensors/temp", "{}");
    let matches = engine.match_trigger(&event);
    assert!(matches.is_empty());
}

fn channel_event(topic: &str, payload: &str) -> SopEvent {
    SopEvent {
        source: SopTriggerSource::Channel,
        topic: Some(topic.into()),
        payload: Some(payload.into()),
        timestamp: now_iso8601(),
    }
}

fn channel_sop(name: &str, alias: Option<&str>, condition: Option<&str>) -> Sop {
    let mut sop = test_sop(name, SopExecutionMode::Auto, SopPriority::Normal);
    sop.triggers = vec![SopTrigger::Channel {
        channel: "telegram".into(),
        alias: alias.map(str::to_string),
        condition: condition.map(str::to_string),
    }];
    sop
}

#[test]
fn channel_trigger_matches_channel_type_case_insensitive() {
    let engine = engine_with_sops(vec![channel_sop("s1", None, None)]);
    assert_eq!(
        engine.match_trigger(&channel_event("telegram", "{}")).len(),
        1
    );
    assert_eq!(
        engine.match_trigger(&channel_event("Telegram", "{}")).len(),
        1
    );
    assert!(
        engine
            .match_trigger(&channel_event("discord", "{}"))
            .is_empty()
    );
}

#[test]
fn channel_trigger_without_alias_matches_any_instance() {
    let engine = engine_with_sops(vec![channel_sop("s1", None, None)]);
    assert_eq!(
        engine
            .match_trigger(&channel_event("telegram/prod", "{}"))
            .len(),
        1
    );
    assert_eq!(
        engine.match_trigger(&channel_event("telegram", "{}")).len(),
        1
    );
}

#[test]
fn channel_trigger_with_alias_requires_exact_alias() {
    let engine = engine_with_sops(vec![channel_sop("s1", Some("prod"), None)]);
    assert_eq!(
        engine
            .match_trigger(&channel_event("telegram/prod", "{}"))
            .len(),
        1
    );
    assert!(
        engine
            .match_trigger(&channel_event("telegram/backup", "{}"))
            .is_empty()
    );
    assert!(
        engine
            .match_trigger(&channel_event("telegram", "{}"))
            .is_empty(),
        "aliased trigger must not match an alias-less topic"
    );
}

#[test]
fn channel_trigger_without_topic_fails_closed() {
    let engine = engine_with_sops(vec![channel_sop("s1", None, None)]);
    let event = SopEvent {
        source: SopTriggerSource::Channel,
        topic: None,
        payload: None,
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&event).is_empty());
}

#[test]
fn channel_trigger_condition_filters_by_payload() {
    let engine = engine_with_sops(vec![channel_sop("s1", None, Some("$.kind == \"deploy\""))]);
    assert_eq!(
        engine
            .match_trigger(&channel_event("telegram", "{\"kind\":\"deploy\"}"))
            .len(),
        1
    );
    assert!(
        engine
            .match_trigger(&channel_event("telegram", "{\"kind\":\"chat\"}"))
            .is_empty()
    );
}

#[test]
fn wants_source_reflects_loaded_trigger_sources() {
    let engine = engine_with_sops(vec![channel_sop("s1", None, None)]);
    assert!(engine.wants_source(SopTriggerSource::Channel));
    assert!(!engine.wants_source(SopTriggerSource::Mqtt));
    assert!(!engine.wants_source(SopTriggerSource::Amqp));

    let empty = engine_with_sops(vec![]);
    assert!(!empty.wants_source(SopTriggerSource::Channel));
}

fn amqp_event(routing_key: &str, payload: &str) -> SopEvent {
    SopEvent {
        source: SopTriggerSource::Amqp,
        topic: Some(routing_key.into()),
        payload: Some(payload.into()),
        timestamp: now_iso8601(),
    }
}

#[test]
fn match_amqp_trigger_wildcard() {
    let sop = Sop {
        triggers: vec![SopTrigger::Amqp {
            routing_key: "org.*.anitya.#".into(),
            condition: None,
        }],
        ..test_sop("anitya-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);
    let hit = engine.match_trigger(&amqp_event(
        "org.release-monitoring.anitya.project.version.update",
        "{}",
    ));
    assert_eq!(hit.len(), 1);
    let miss = engine.match_trigger(&amqp_event("org.release-monitoring.fedmsg.x", "{}"));
    assert!(miss.is_empty());
}

#[test]
fn match_mqtt_trigger_exact() {
    let sop = Sop {
        triggers: vec![SopTrigger::Mqtt {
            topic: "plant/pump/pressure".into(),
            condition: None,
        }],
        ..test_sop(
            "pressure-sop",
            SopExecutionMode::Auto,
            SopPriority::Critical,
        )
    };
    let engine = engine_with_sops(vec![sop]);
    let matches = engine.match_trigger(&mqtt_event("plant/pump/pressure", "87.3"));
    assert_eq!(matches.len(), 1);
}

#[test]
fn match_mqtt_wildcard_plus() {
    let sop = Sop {
        triggers: vec![SopTrigger::Mqtt {
            topic: "plant/+/pressure".into(),
            condition: None,
        }],
        ..test_sop("wildcard-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);
    assert_eq!(
        engine
            .match_trigger(&mqtt_event("plant/pump_3/pressure", "87"))
            .len(),
        1
    );
    assert!(
        engine
            .match_trigger(&mqtt_event("plant/pump_3/temperature", "50"))
            .is_empty()
    );
}

#[test]
fn match_mqtt_wildcard_hash() {
    let sop = Sop {
        triggers: vec![SopTrigger::Mqtt {
            topic: "plant/#".into(),
            condition: None,
        }],
        ..test_sop("hash-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);
    assert_eq!(
        engine
            .match_trigger(&mqtt_event("plant/pump/pressure", "87"))
            .len(),
        1
    );
    assert_eq!(
        engine
            .match_trigger(&mqtt_event("plant/a/b/c/d", "x"))
            .len(),
        1
    );
}

// ── Calendar trigger matching ─────────────────────

fn calendar_event(topic: Option<&str>, calendar_source: &str, calendar_id: &str) -> SopEvent {
    let now = chrono::Utc::now();
    SopEvent {
        source: SopTriggerSource::Calendar,
        topic: topic.map(str::to_string),
        payload: Some(
            serde_json::json!({
                "event_id": "evt-1",
                "event_title": "Standup",
                "expected_start": now,
                "detected_at": now,
                "calendar_source": calendar_source,
                "calendar_id": calendar_id,
            })
            .to_string(),
        ),
        timestamp: now_iso8601(),
    }
}

#[test]
fn calendar_trigger_matches_source_and_any_calendar_when_ids_empty() {
    let sop = Sop {
        triggers: vec![SopTrigger::Calendar {
            calendar_source: "microsoft365".into(),
            calendar_ids: Vec::new(),
            condition: None,
        }],
        ..test_sop("calendar-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    let matches = engine.match_trigger(&calendar_event(
        Some(CALENDAR_NO_SHOW_TOPIC),
        "microsoft365",
        "team",
    ));

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "calendar-sop");
}

#[test]
fn calendar_trigger_filters_calendar_ids_and_source() {
    let sop = Sop {
        triggers: vec![SopTrigger::Calendar {
            calendar_source: "microsoft365".into(),
            calendar_ids: vec!["primary".into()],
            condition: None,
        }],
        ..test_sop("calendar-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    assert_eq!(
        engine
            .match_trigger(&calendar_event(
                Some(CALENDAR_NO_SHOW_TOPIC),
                "microsoft365",
                "primary"
            ))
            .len(),
        1
    );
    assert!(
        engine
            .match_trigger(&calendar_event(
                Some(CALENDAR_NO_SHOW_TOPIC),
                "microsoft365",
                "team"
            ))
            .is_empty()
    );
    assert!(
        engine
            .match_trigger(&calendar_event(
                Some(CALENDAR_NO_SHOW_TOPIC),
                "google",
                "primary"
            ))
            .is_empty()
    );
}

#[test]
fn calendar_trigger_requires_no_show_topic_and_valid_payload() {
    let sop = Sop {
        triggers: vec![SopTrigger::Calendar {
            calendar_source: "microsoft365".into(),
            calendar_ids: Vec::new(),
            condition: None,
        }],
        ..test_sop("calendar-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    assert!(
        engine
            .match_trigger(&calendar_event(
                Some("calendar.updated"),
                "microsoft365",
                "primary"
            ))
            .is_empty()
    );

    let invalid_payload_event = SopEvent {
        source: SopTriggerSource::Calendar,
        topic: Some(CALENDAR_NO_SHOW_TOPIC.into()),
        payload: Some("not json".into()),
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&invalid_payload_event).is_empty());

    let missing_payload_event = SopEvent {
        source: SopTriggerSource::Calendar,
        topic: Some(CALENDAR_NO_SHOW_TOPIC.into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&missing_payload_event).is_empty());

    let malformed_payload_event = SopEvent {
        source: SopTriggerSource::Calendar,
        topic: Some(CALENDAR_NO_SHOW_TOPIC.into()),
        payload: Some(
            serde_json::json!({
                "event_id": "evt-1",
                "event_title": "Standup",
                "expected_start": chrono::Utc::now(),
                "detected_at": chrono::Utc::now(),
                "calendar_source": "microsoft365",
                "calendar_id": 17,
            })
            .to_string(),
        ),
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&malformed_payload_event).is_empty());
}

// ── Webhook trigger matching ─────────────────────

#[test]
fn webhook_trigger_matches_exact_path() {
    let sop = Sop {
        triggers: vec![SopTrigger::Webhook {
            path: "/webhook".into(),
        }],
        ..test_sop("webhook-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    // Exact match — should match
    let event = SopEvent {
        source: SopTriggerSource::Webhook,
        topic: Some("/webhook".into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert_eq!(engine.match_trigger(&event).len(), 1);
}

#[test]
fn webhook_trigger_rejects_different_path() {
    let sop = Sop {
        triggers: vec![SopTrigger::Webhook {
            path: "/sop/deploy".into(),
        }],
        ..test_sop("deploy-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    // Path /webhook does NOT match /sop/deploy
    let event = SopEvent {
        source: SopTriggerSource::Webhook,
        topic: Some("/webhook".into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&event).is_empty());

    // But /sop/deploy matches /sop/deploy
    let event = SopEvent {
        source: SopTriggerSource::Webhook,
        topic: Some("/sop/deploy".into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert_eq!(engine.match_trigger(&event).len(), 1);
}

#[test]
fn channel_trigger_matches_forge_topic_and_condition() {
    let sop = Sop {
        triggers: vec![SopTrigger::Channel {
            channel: "git".into(),
            alias: Some("main".into()),
            condition: Some("$.event_type == \"pull_request.opened\"".into()),
        }],
        ..test_sop("git-pr-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    let event = SopEvent {
        source: SopTriggerSource::Channel,
        topic: Some("git.main:pull_request.opened".into()),
        payload: Some(
            r#"{"event_type":"pull_request.opened","repo":"octo/repo","number":12}"#.into(),
        ),
        timestamp: now_iso8601(),
    };
    assert_eq!(engine.match_trigger(&event).len(), 1);

    let wrong_event_type = SopEvent {
        source: SopTriggerSource::Channel,
        topic: Some("git.main:issues.opened".into()),
        payload: Some(r#"{"event_type":"issues.opened","repo":"octo/repo"}"#.into()),
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&wrong_event_type).is_empty());

    let wrong_alias = SopEvent {
        source: SopTriggerSource::Channel,
        topic: Some("git.staging:pull_request.opened".into()),
        payload: Some(r#"{"event_type":"pull_request.opened","repo":"octo/repo"}"#.into()),
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&wrong_alias).is_empty());
}

// ── Cron trigger matching ─────────────────────────

#[test]
fn cron_trigger_matches_only_matching_expression() {
    let sop = Sop {
        triggers: vec![SopTrigger::Cron {
            expression: "0 */5 * * *".into(),
        }],
        ..test_sop("cron-sop", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    // Matching expression
    let event = SopEvent {
        source: SopTriggerSource::Cron,
        topic: Some("0 */5 * * *".into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert_eq!(engine.match_trigger(&event).len(), 1);

    // Different expression — should NOT match
    let event = SopEvent {
        source: SopTriggerSource::Cron,
        topic: Some("0 */10 * * *".into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&event).is_empty());

    // No topic — should NOT match
    let event = SopEvent {
        source: SopTriggerSource::Cron,
        topic: None,
        payload: None,
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&event).is_empty());
}

// ── Condition-based trigger matching ────────────────

#[test]
fn mqtt_condition_filters_by_payload() {
    let sop = Sop {
        triggers: vec![SopTrigger::Mqtt {
            topic: "sensors/pressure".into(),
            condition: Some("$.value > 85".into()),
        }],
        ..test_sop("cond-sop", SopExecutionMode::Auto, SopPriority::Critical)
    };
    let engine = engine_with_sops(vec![sop]);

    // Payload meets condition
    let matches = engine.match_trigger(&mqtt_event("sensors/pressure", r#"{"value": 90}"#));
    assert_eq!(matches.len(), 1);

    // Payload does not meet condition
    let matches = engine.match_trigger(&mqtt_event("sensors/pressure", r#"{"value": 50}"#));
    assert!(matches.is_empty());
}

#[test]
fn mqtt_no_condition_matches_any_payload() {
    let sop = Sop {
        triggers: vec![SopTrigger::Mqtt {
            topic: "sensors/temp".into(),
            condition: None,
        }],
        ..test_sop("no-cond", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    let matches = engine.match_trigger(&mqtt_event("sensors/temp", "anything"));
    assert_eq!(matches.len(), 1);
}

#[test]
fn mqtt_condition_no_payload_fails_closed() {
    let sop = Sop {
        triggers: vec![SopTrigger::Mqtt {
            topic: "sensors/temp".into(),
            condition: Some("$.value > 0".into()),
        }],
        ..test_sop("no-payload", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    // Event with no payload
    let event = SopEvent {
        source: SopTriggerSource::Mqtt,
        topic: Some("sensors/temp".into()),
        payload: None,
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&event).is_empty());
}

#[test]
fn peripheral_condition_filters_by_payload() {
    let sop = Sop {
        triggers: vec![SopTrigger::Peripheral {
            board: "nucleo".into(),
            signal: "pin_3".into(),
            condition: Some("> 0".into()),
        }],
        ..test_sop("periph-cond", SopExecutionMode::Auto, SopPriority::High)
    };
    let engine = engine_with_sops(vec![sop]);

    // Positive signal
    let event = SopEvent {
        source: SopTriggerSource::Peripheral,
        topic: Some("nucleo/pin_3".into()),
        payload: Some("1".into()),
        timestamp: now_iso8601(),
    };
    assert_eq!(engine.match_trigger(&event).len(), 1);

    // Zero signal — does not meet condition
    let event = SopEvent {
        source: SopTriggerSource::Peripheral,
        topic: Some("nucleo/pin_3".into()),
        payload: Some("0".into()),
        timestamp: now_iso8601(),
    };
    assert!(engine.match_trigger(&event).is_empty());
}

#[test]
fn peripheral_no_condition_matches_any() {
    let sop = Sop {
        triggers: vec![SopTrigger::Peripheral {
            board: "rpi".into(),
            signal: "gpio_5".into(),
            condition: None,
        }],
        ..test_sop("periph-nocond", SopExecutionMode::Auto, SopPriority::Normal)
    };
    let engine = engine_with_sops(vec![sop]);

    let event = SopEvent {
        source: SopTriggerSource::Peripheral,
        topic: Some("rpi/gpio_5".into()),
        payload: Some("0".into()),
        timestamp: now_iso8601(),
    };
    assert_eq!(engine.match_trigger(&event).len(), 1);
}

// ── Run lifecycle ───────────────────────────────────

#[test]
fn start_run_returns_first_step() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action);
    assert!(run_id.starts_with("run-"));
    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));
    assert_eq!(engine.active_runs().len(), 1);
}

#[test]
fn run_notifier_publishes_on_admission() {
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )])
    .with_run_notifier(tx);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action);
    let published = rx
        .try_recv()
        .expect("a summary must be published on admission");
    assert_eq!(published.run_id, run_id);
    assert_eq!(published.sop_name, "s1");
    assert!(published.active, "an admitted run is active");
}

#[test]
fn run_notifier_absent_is_a_noop() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    assert!(engine.subscribe_run_changes().is_none());
    engine.start_run("s1", manual_event()).unwrap();
    assert_eq!(engine.active_runs().len(), 1);
}

#[test]
fn start_run_unknown_sop_fails() {
    let mut engine = engine_with_sops(vec![]);
    assert!(engine.start_run("nonexistent", manual_event()).is_err());
}

#[test]
fn advance_step_to_completion() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Complete step 1
    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "done".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    // Should get step 2
    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));

    // Complete step 2
    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "done".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(matches!(action, SopRunAction::Completed { .. }));
    assert!(engine.active_runs().is_empty());
    assert_eq!(engine.finished_runs(None).len(), 1);
}

#[test]
fn step_failure_ends_run() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Failed,
                output: "valve stuck".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::Failed { ref reason, .. } if reason.contains("valve stuck"))
    );
    assert!(engine.active_runs().is_empty());
}

#[test]
fn schema_input_failure_fails_run_before_first_action() {
    let mut sop = test_sop("schema-in", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].schema = Some(StepSchema {
        input: Some(required_object_schema("ok")),
        output: None,
    });
    let mut engine = engine_with_sops(vec![sop]);
    let event = SopEvent {
        source: SopTriggerSource::Manual,
        topic: None,
        payload: Some("{}".into()),
        timestamp: now_iso8601(),
    };

    let action = engine.start_run("schema-in", event).unwrap();
    let run_id = extract_run_id(&action).to_string();

    assert!(
        matches!(action, SopRunAction::Failed { ref reason, .. } if reason.contains("input schema validation failed"))
    );
    let events = engine.run_events(&run_id).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "step_schema_reject"
            && event.payload["step"] == serde_json::json!(1)
            && event.payload["phase"] == serde_json::json!("input")
    }));
    assert!(engine.active_runs().is_empty());
    assert_eq!(engine.finished_runs(None)[0].status, SopRunStatus::Failed);
}

#[test]
fn start_run_terminal_persist_failure_retains_run_and_claim() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(true),
    });
    let mut sop = test_sop(
        "schema-start-finish-fail",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    );
    sop.steps[0].schema = Some(StepSchema {
        input: Some(required_object_schema("ok")),
        output: None,
    });
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let err = engine
        .start_run(
            "schema-start-finish-fail",
            SopEvent {
                source: SopTriggerSource::Manual,
                topic: None,
                payload: Some("{}".into()),
                timestamp: now_iso8601(),
            },
        )
        .expect_err("terminal persistence failure must reject start");

    assert!(err.is::<TerminalPersistenceRetained>());
    assert!(err.to_string().contains("injected finish failure"));
    let run_id = first_active_run_id(&engine);
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Running,
        "failed terminal persistence must leave the start-path run active"
    );
    assert_eq!(
        store.claim_counts("schema-start-finish-fail").unwrap(),
        (1, 1),
        "failed terminal persistence must keep the admission claim"
    );
    assert!(
        engine.finished_runs(None).is_empty(),
        "the run must not move to terminal cache until terminal persistence succeeds"
    );
}

#[test]
fn start_deterministic_terminal_persist_failure_retains_run_and_claim() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(true),
    });
    let mut sop = deterministic_sop_all_execute("det-schema-start-finish-fail");
    sop.steps[0].schema = Some(StepSchema {
        input: Some(required_object_schema("ok")),
        output: None,
    });
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let err = engine
        .start_deterministic_run("det-schema-start-finish-fail", manual_event())
        .expect_err("terminal persistence failure must reject deterministic start");

    assert!(err.is::<TerminalPersistenceRetained>());
    assert!(err.to_string().contains("injected finish failure"));
    let run_id = first_active_run_id(&engine);
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Running,
        "failed terminal persistence must leave the deterministic run active"
    );
    assert_eq!(
        store.claim_counts("det-schema-start-finish-fail").unwrap(),
        (1, 1),
        "failed terminal persistence must keep the deterministic admission claim"
    );
    assert!(
        engine.finished_runs(None).is_empty(),
        "the deterministic run must not move to terminal cache until persistence succeeds"
    );
}

#[test]
fn schema_output_failure_fails_run_before_next_step() {
    let mut sop = test_sop("schema-out", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].schema = Some(StepSchema {
        input: None,
        output: Some(required_object_schema("ok")),
    });
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("schema-out", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "{}".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::Failed { ref reason, .. } if reason.contains("output schema validation failed"))
    );
    let events = engine.run_events(&run_id).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "step_schema_reject"
            && event.payload["step"] == serde_json::json!(1)
            && event.payload["phase"] == serde_json::json!("output")
    }));
    assert!(engine.active_runs().is_empty());
    assert_eq!(engine.finished_runs(None)[0].status, SopRunStatus::Failed);
}

#[test]
fn schema_enforcement_disabled_allows_invalid_output() {
    let mut sop = test_sop("schema-off", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].schema = Some(StepSchema {
        input: None,
        output: Some(required_object_schema("ok")),
    });
    let config = SopConfig {
        step_schema_enforce: false,
        ..SopConfig::default()
    };
    let mut engine = engine_with_config_sops(config, vec![sop]);
    let action = engine.start_run("schema-off", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "{}".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));
    assert_eq!(engine.active_runs()[&run_id].current_step, 2);
}

#[test]
fn explicit_next_routes_llm_run_over_linear_successor() {
    let mut sop = test_sop("route-next", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps.push(SopStep {
        number: 3,
        title: "Step three".into(),
        body: "Do step three".into(),
        ..SopStep::default()
    });
    sop.steps[0].routing.next = Some(3);
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("route-next", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: r#"{"ok":true}"#.into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::ExecuteStep { ref step, .. } if step.number == 3),
        "explicit routing should select step 3 instead of the linear step 2"
    );
    let events = engine.run_events(&run_id).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "step_promoted"
            && event.payload["from_step"] == serde_json::json!(1)
            && event.payload["to_step"] == serde_json::json!(3)
    }));
    assert_eq!(engine.active_runs()[&run_id].current_step, 3);
}

#[test]
fn failed_step_retries_until_policy_limit() {
    let mut sop = test_sop("route-retry", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].on_failure = StepFailure::Retry { max: 2 };
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("route-retry", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Failed,
                output: "first failure".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::ExecuteStep { ref step, .. } if step.number == 1),
        "initial failed attempt should allow the first retry of step 1"
    );
    let events = engine.run_events(&run_id).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "step_retry" && event.payload["step"] == serde_json::json!(1)
    }));
    assert_eq!(engine.active_runs()[&run_id].current_step, 1);

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Failed,
                output: "second failure".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::ExecuteStep { ref step, .. } if step.number == 1),
        "first failed retry should allow the second retry of step 1"
    );
    assert_eq!(engine.active_runs()[&run_id].current_step, 1);

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Failed,
                output: "third failure".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::Failed { ref reason, .. } if reason.contains("retry limit"))
    );
    assert!(engine.active_runs().is_empty());
}

#[test]
fn failed_step_goto_routes_to_compensating_step() {
    let mut sop = test_sop("route-goto", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].on_failure = StepFailure::Goto { step: 2 };
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("route-goto", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Failed,
                output: "needs compensation".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(matches!(action, SopRunAction::ExecuteStep { ref step, .. } if step.number == 2));
    assert_eq!(engine.active_runs()[&run_id].current_step, 2);
}

#[test]
fn ineligible_routed_step_is_marked_skipped_and_pending() {
    let mut sop = test_sop("route-pending", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[1].routing.depends_on = vec![42];
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("route-pending", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: r#"{"ok":true}"#.into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::Pending { step: 2, ref reason, .. } if reason.contains("dependencies"))
    );
    let run = &engine.active_runs()[&run_id];
    assert_eq!(run.status, SopRunStatus::Pending);
    assert_eq!(run.current_step, 2);
    assert!(
        run.step_results
            .iter()
            .any(|result| result.step_number == 2 && result.status == SopStepStatus::Skipped)
    );
    let events = engine.run_events(&run_id).unwrap();
    assert!(events.iter().any(|event| {
        event.kind == "step_skipped"
            && event.payload["step"] == serde_json::json!(2)
            && event.payload["status"] == serde_json::json!("pending")
    }));
}

#[test]
fn output_schema_failure_can_retry_through_on_failure_policy() {
    let mut sop = test_sop("schema-retry", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].schema = Some(StepSchema {
        input: None,
        output: Some(required_object_schema("ok")),
    });
    sop.steps[0].on_failure = StepFailure::Retry { max: 2 };
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("schema-retry", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "{}".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(
        matches!(action, SopRunAction::ExecuteStep { ref step, .. } if step.number == 1),
        "schema output failure should route through on_failure retry"
    );

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: r#"{"ok":true}"#.into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(matches!(action, SopRunAction::ExecuteStep { ref step, .. } if step.number == 2));
}

#[test]
fn cancel_run() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine.cancel_run(&run_id).unwrap();
    assert!(engine.active_runs().is_empty());
    let finished = engine.finished_runs(None);
    assert_eq!(finished[0].status, SopRunStatus::Cancelled);
}

#[test]
fn cancel_unknown_run_fails() {
    let mut engine = engine_with_sops(vec![]);
    assert!(engine.cancel_run("nonexistent").is_err());
}

#[test]
fn finish_unknown_run_returns_error_without_mutating_engine() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);

    let error = engine
        .finish_run("nonexistent", SopRunStatus::Failed, Some("failed".into()))
        .expect_err("finishing an unknown run must return an error");

    assert!(
        error
            .to_string()
            .contains("Active run not found: nonexistent")
    );
    assert!(engine.active_runs().is_empty());
    assert!(engine.finished_runs(None).is_empty());

    let action = engine
        .start_run("s1", manual_event())
        .expect("the engine must remain usable after an unknown finish");
    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));
}

// ── Concurrency ─────────────────────────────────────

#[test]
fn per_sop_concurrency_limit() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    // max_concurrent = 1 by default
    engine.start_run("s1", manual_event()).unwrap();
    assert!(!engine.can_start("s1"));
    assert!(engine.start_run("s1", manual_event()).is_err());
}

#[test]
fn global_concurrency_limit() {
    let sops = vec![
        test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal),
        test_sop("s2", SopExecutionMode::Auto, SopPriority::Normal),
    ];
    let mut engine = SopEngine::new(SopConfig {
        max_concurrent_total: 1,
        ..SopConfig::default()
    });
    engine.sops = sops;

    engine.start_run("s1", manual_event()).unwrap();
    assert!(!engine.can_start("s2"));
}

#[test]
fn start_run_uses_store_claims_across_engine_instances() {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let sops = vec![test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal)];
    let mut first = engine_with_sops(sops.clone()).with_store(store.clone());
    let mut second = engine_with_sops(sops).with_store(store.clone());

    let action = first.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    assert!(
        !second.can_start("s1"),
        "read-only admission check must see the shared store claim"
    );
    assert!(
        second.start_run("s1", manual_event()).is_err(),
        "CAS claim must block a second engine with an empty local active map"
    );

    first.cancel_run(&run_id).unwrap();
    assert!(
        second.can_start("s1"),
        "finishing the first run releases the shared claim slot"
    );
    assert!(second.start_run("s1", manual_event()).is_ok());
}

#[test]
fn pending_pool_cap_is_shared_across_engines_via_store() {
    // `max_pending_approvals` must bound the pending pool across ALL engine
    // holders of the shared store, not just this process's local active map. A
    // run parked at approval by one engine (persisted, exec claim released) must
    // count against a second engine's admission decision - otherwise two engines
    // sharing a store admit past the cap.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 5; // exec slots are not the limiter here...
    sop.max_pending_approvals = 1; // ...the pending-approval pool is.
    let sops = vec![sop];
    let mut first = engine_with_sops(sops.clone()).with_store(store.clone());
    let second = engine_with_sops(sops).with_store(store.clone());

    // First engine parks a run at approval (releases its exec claim, persists).
    let action = first.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert_eq!(
        first.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval
    );

    // Second engine's LOCAL active map is empty, yet the shared store shows the
    // parked run, so the pending pool reads full -> the trigger is deferred, not
    // admitted past the cap.
    assert!(
        second.active_runs.is_empty(),
        "second engine has no local runs"
    );
    assert!(
        matches!(second.evaluate_admission("s1"), SopAdmission::Defer { .. }),
        "a sibling engine's persisted pending run must count against the cap"
    );
}

#[test]
fn current_step_policy_name_matches_step_number_not_index() {
    // B#2: a routed SOP with NON-CONTIGUOUS step numbers. The policy lookup must
    // match the step whose `number` == current_step, not the step at that vec
    // index - otherwise a positional read silently unpolices (or mis-polices) the
    // gate.
    let mut engine = engine_with_sops(vec![]);
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.steps = vec![
        SopStep {
            number: 1,
            policy: None,
            ..SopStep::default()
        },
        SopStep {
            number: 5,
            policy: Some("prod".into()),
            ..SopStep::default()
        },
    ];
    engine.set_sops_for_test(vec![sop]);
    let now = now_iso8601();
    engine.active_runs.insert(
        "r1".to_string(),
        SopRun {
            run_id: "r1".to_string(),
            sop_name: "s1".to_string(),
            trigger_event: manual_event(),
            frame_marker_id: "m".to_string(),
            status: SopRunStatus::WaitingApproval,
            current_step: 5,
            total_steps: 2,
            started_at: now.clone(),
            completed_at: None,
            step_results: Vec::new(),
            waiting_since: Some(now),
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        },
    );
    assert_eq!(
        engine.current_step_policy_name("r1").as_deref(),
        Some("prod"),
        "policy resolves by step number (5), not vec index"
    );
}

#[test]
fn current_step_policy_name_treats_empty_or_whitespace_as_none() {
    // A TOML `policy = ""` step deserializes to `Some("")` (types.rs has no empty
    // normalization, unlike the Markdown parser's `policy:` bullet in mod.rs).
    // Without normalizing here, the broker would treat "" as a NAMED-but-absent
    // policy and fail closed (gate stuck waiting forever) - diverging from the
    // equivalent Markdown SOP, which normalizes empty to unpoliced (`None`).
    let mut engine = engine_with_sops(vec![]);
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.steps = vec![
        SopStep {
            number: 1,
            policy: Some(String::new()),
            ..SopStep::default()
        },
        SopStep {
            number: 2,
            policy: Some("   ".into()),
            ..SopStep::default()
        },
    ];
    engine.set_sops_for_test(vec![sop]);
    let now = now_iso8601();
    for (run_id, step) in [("r1", 1u32), ("r2", 2u32)] {
        engine.active_runs.insert(
            run_id.to_string(),
            SopRun {
                run_id: run_id.to_string(),
                sop_name: "s1".to_string(),
                trigger_event: manual_event(),
                frame_marker_id: "m".to_string(),
                status: SopRunStatus::WaitingApproval,
                current_step: step,
                total_steps: 2,
                started_at: now.clone(),
                completed_at: None,
                step_results: Vec::new(),
                waiting_since: Some(now.clone()),
                llm_calls_saved: 0,
                revision: 0,
                revision_base: 0,
            },
        );
    }
    assert_eq!(
        engine.current_step_policy_name("r1"),
        None,
        "empty-string policy name normalizes to unpoliced, matching Markdown"
    );
    assert_eq!(
        engine.current_step_policy_name("r2"),
        None,
        "whitespace-only policy name also normalizes to unpoliced"
    );
}

#[test]
fn capability_step_execution_increments_the_capability_executed_metric() {
    // record_capability_executed is called unconditionally in
    // execute_capability_step, before the result is inspected - so the counter
    // means "attempted", not "succeeded". Proves both the global and per-SOP
    // counters increment, and that a failing capability still counts as attempted.
    let metrics = std::sync::Arc::new(super::super::metrics::SopMetricsCollector::new());
    let mut engine = engine_with_sops(vec![]).with_metrics(metrics.clone());
    let sop = test_sop("s1", SopExecutionMode::Deterministic, SopPriority::Normal);
    engine.set_sops_for_test(vec![sop.clone()]);
    let now = now_iso8601();
    engine.active_runs.insert(
        "r1".to_string(),
        SopRun {
            run_id: "r1".to_string(),
            sop_name: "s1".to_string(),
            trigger_event: manual_event(),
            frame_marker_id: "m".to_string(),
            status: SopRunStatus::Running,
            current_step: 1,
            total_steps: 1,
            started_at: now.clone(),
            completed_at: None,
            step_results: Vec::new(),
            waiting_since: None,
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        },
    );
    let step = SopStep {
        number: 1,
        kind: SopStepKind::Capability,
        capability: Some("noop".into()),
        ..SopStep::default()
    };
    engine
        .execute_capability_step(&sop, "r1", &step, serde_json::json!({}))
        .expect("noop capability always succeeds");
    assert_eq!(
        metrics.get_metric_value("sop.capability_executed"),
        Some(serde_json::json!(1)),
        "global counter increments on capability execution"
    );
    assert_eq!(
        metrics.get_metric_value("sop.s1.capability_executed"),
        Some(serde_json::json!(1)),
        "per-SOP counter increments too"
    );
}

#[test]
fn gate_votes_are_per_step_and_canonical_per_subject() {
    // The broker tallies quorum from gate_votes_for_step(run_id, step). Votes are
    // scoped to the current step (a two-gate SOP does not reuse step-1 votes), and
    // the stored voter key is the CANONICAL subject: HTTP and WS share the paired
    // credential, so the same subject over both transports records ONE voter_key
    // (cannot inflate quorum), while a genuinely different source (CLI) is distinct.
    use crate::sop::approval::ApprovalPrincipal;
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let engine = engine_with_sops(vec![]).with_store(store);

    // Same subject "ZeroClawOperator" over HTTP then WS: collapses to gateway:ZeroClawOperator.
    engine
        .record_gate_vote(
            "run-1",
            1,
            "p",
            0,
            &ApprovalPrincipal::http(Some("ZeroClawOperator".into())),
        )
        .unwrap();
    engine
        .record_gate_vote(
            "run-1",
            1,
            "p",
            0,
            &ApprovalPrincipal::ws("c".into(), Some("ZeroClawOperator".into())),
        )
        .unwrap();
    // A repeat over HTTP: still the same canonical voter.
    engine
        .record_gate_vote(
            "run-1",
            1,
            "p",
            0,
            &ApprovalPrincipal::http(Some("ZeroClawOperator".into())),
        )
        .unwrap();
    // A CLI actor is a genuinely distinct source (cli:ZeroClawMaintainer).
    engine
        .record_gate_vote(
            "run-1",
            1,
            "p",
            0,
            &ApprovalPrincipal::cli(Some("ZeroClawMaintainer".into())),
        )
        .unwrap();
    // A vote on step 2 is a separate tally.
    engine
        .record_gate_vote(
            "run-1",
            2,
            "p",
            0,
            &ApprovalPrincipal::cli(Some("carol".into())),
        )
        .unwrap();

    // Engine surfaces the raw rows; the distinct voter_key count is the broker's
    // dedup, reproduced here to prove per-step scoping + subject canonicalization.
    let distinct = |step| {
        engine
            .gate_votes_for_step("run-1", step)
            .unwrap()
            .into_iter()
            .map(|v| v.voter_key)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    };
    assert_eq!(
        distinct(1),
        2,
        "gateway:ZeroClawOperator (http+ws collapsed) + cli:ZeroClawMaintainer = 2 distinct step-1 voters"
    );
    assert_eq!(
        distinct(2),
        1,
        "step-2 quorum does not include step-1 voters"
    );
    assert_eq!(distinct(3), 0, "no votes recorded for step 3");
}

#[test]
fn record_gate_vote_is_idempotent_per_voter_and_policy() {
    // A repeat vote by the same voter under the same policy must not grow the
    // append-only ledger (the count already dedups by voter_key; this keeps a
    // retry from writing duplicate rows). A different policy is a distinct row.
    use crate::sop::approval::ApprovalPrincipal;
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let engine = engine_with_sops(vec![]).with_store(store);
    let zero_claw_operator = ApprovalPrincipal::cli(Some("ZeroClawOperator".into()));

    engine
        .record_gate_vote("run-1", 1, "prod", 0, &zero_claw_operator)
        .unwrap();
    engine
        .record_gate_vote("run-1", 1, "prod", 0, &zero_claw_operator)
        .unwrap();
    assert_eq!(
        engine.gate_votes_for_step("run-1", 1).unwrap().len(),
        1,
        "a repeat vote by the same voter under the same policy must not append a duplicate row"
    );

    engine
        .record_gate_vote("run-1", 1, "prod2", 0, &zero_claw_operator)
        .unwrap();
    assert_eq!(
        engine.gate_votes_for_step("run-1", 1).unwrap().len(),
        2,
        "a vote under a different policy is a distinct row"
    );
}

#[test]
fn pending_pool_cap_is_enforced_when_active_runs_reach_later_approval() {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.max_concurrent = 2;
    sop.max_pending_approvals = 1;
    sop.steps[1].requires_confirmation = true;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let first = engine.start_run("s1", manual_event()).unwrap();
    let first_id = extract_run_id(&first).to_string();
    let second = engine.start_run("s1", manual_event()).unwrap();
    let second_id = extract_run_id(&second).to_string();
    assert_eq!(store.claim_counts("s1").unwrap(), (2, 2));

    let first_gate = engine
        .advance_step(
            &first_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "first".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(matches!(first_gate, SopRunAction::WaitApproval { .. }));
    assert_eq!(
        engine.get_run(&first_id).unwrap().status,
        SopRunStatus::WaitingApproval
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the first parked run released its exec claim"
    );
    assert_eq!(engine.pending_count_for_sop("s1"), 1);

    let second_blocked = engine
        .advance_step(
            &second_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "second".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        matches!(
            second_blocked,
            SopRunAction::Pending { step: 2, ref reason, .. }
                if reason.contains("pending-approval pool full")
        ),
        "second run must not park past max_pending_approvals"
    );
    assert_eq!(
        engine.get_run(&second_id).unwrap().status,
        SopRunStatus::Pending
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the pending second run keeps its exec claim instead of parking claimless"
    );
    assert_eq!(
        engine.pending_count_for_sop("s1"),
        1,
        "only the first run counts against the pending approval pool"
    );
    let skipped = engine
        .advance_step(
            &second_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "unauthorized".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .expect_err("pending approval-cap backpressure must not be advanceable");
    assert!(
        skipped.to_string().contains("pending at gated step"),
        "unexpected advance error: {skipped}"
    );
    assert_eq!(
        engine.get_run(&second_id).unwrap().status,
        SopRunStatus::Pending,
        "the capped approval gate remains pending and cannot be bypassed"
    );
    let first_resumed = engine
        .resolve_gate(
            &first_id,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(matches!(first_resumed, ResolveOutcome::Resumed(_)));

    engine.run_maintenance_tick();
    assert_eq!(
        engine.get_run(&second_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "maintenance retries the blocked approval gate once pending capacity frees"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the recovered second gate releases its kept claim while waiting"
    );

    let second_resumed = engine
        .resolve_gate(
            &second_id,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(matches!(second_resumed, ResolveOutcome::Resumed(_)));
}

#[test]
fn pending_checkpoint_cap_cannot_be_advanced_without_gate() {
    let mut sop = deterministic_sop("det-cp-cap");
    sop.max_concurrent = 2;
    sop.max_pending_approvals = 1;
    let mut engine = engine_with_sops(vec![sop]);

    let first = engine
        .start_deterministic_run("det-cp-cap", manual_event())
        .unwrap();
    let first_id = extract_run_id(&first).to_string();
    let second = engine
        .start_deterministic_run("det-cp-cap", manual_event())
        .unwrap();
    let second_id = extract_run_id(&second).to_string();

    let first_checkpoint = engine
        .advance_deterministic_step(&first_id, serde_json::json!("first"), None)
        .unwrap();
    assert!(matches!(
        first_checkpoint,
        SopRunAction::CheckpointWait { .. }
    ));

    let second_blocked = engine
        .advance_deterministic_step(&second_id, serde_json::json!("second"), None)
        .unwrap();
    assert!(
        matches!(
            second_blocked,
            SopRunAction::Pending { step: 2, ref reason, .. }
                if reason.contains("pending-approval pool full")
        ),
        "second checkpoint must not park past max_pending_approvals"
    );
    assert_eq!(
        engine.get_run(&second_id).unwrap().status,
        SopRunStatus::Pending
    );

    let skipped = engine
        .advance_step(
            &second_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "unauthorized checkpoint".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .expect_err("pending checkpoint-cap backpressure must not be advanceable");
    assert!(
        skipped.to_string().contains("pending at gated step"),
        "unexpected advance error: {skipped}"
    );
    assert_eq!(
        engine.get_run(&second_id).unwrap().status,
        SopRunStatus::Pending,
        "the capped checkpoint gate remains pending and cannot be bypassed"
    );
    let first_resumed = engine
        .decide_checkpoint(&first_id, ApprovalDecision::Approve)
        .unwrap();
    assert!(matches!(
        first_resumed,
        SopRunAction::DeterministicStep { .. }
    ));

    engine.run_maintenance_tick();
    assert_eq!(
        engine.get_run(&second_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "maintenance retries the blocked checkpoint once pending capacity frees"
    );
    assert_eq!(
        engine.exec_counts("det-cp-cap"),
        (1, 1),
        "the recovered second checkpoint releases its kept claim while paused"
    );

    let second_resumed = engine
        .decide_checkpoint(&second_id, ApprovalDecision::Approve)
        .unwrap();
    assert!(matches!(
        second_resumed,
        SopRunAction::DeterministicStep { .. }
    ));
}

#[test]
fn pending_park_retry_respects_pending_pool_cap() {
    let store = std::sync::Arc::new(FailingSaveLeasedStore::healthy());
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.max_concurrent = 2;
    sop.max_pending_approvals = 1;
    sop.steps[1].requires_confirmation = true;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let first = engine.start_run("s1", manual_event()).unwrap();
    let first_id = extract_run_id(&first).to_string();
    let second = engine.start_run("s1", manual_event()).unwrap();
    let second_id = extract_run_id(&second).to_string();
    assert_eq!(store.claim_counts("s1").unwrap(), (2, 2));

    store.fail_next_save();
    let first_gate = engine
        .advance_step(
            &first_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "first".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        matches!(first_gate, SopRunAction::Pending { ref reason, .. }
            if reason.contains("park snapshot not yet durably persisted")),
        "failed first park persist must surface as durable pending, got {first_gate:?}"
    );
    assert_eq!(
        engine.get_run(&first_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the in-memory gate remains parked while its claim is kept"
    );
    assert!(engine.is_park_persist_pending(&first_id));

    let second_gate = engine
        .advance_step(
            &second_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "second".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(matches!(second_gate, SopRunAction::WaitApproval { .. }));
    assert_eq!(
        engine.pending_count_for_sop("s1"),
        1,
        "the second run fills the durable pending pool before retry"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "only the failed first park still holds an exec claim"
    );

    engine.config.approval_timeout_secs = 1;
    engine.active_runs.get_mut(&first_id).unwrap().waiting_since =
        Some("2000-01-01T00:00:00Z".to_string());
    let summary = engine.run_maintenance_tick();
    assert_eq!(
        summary.timed_out, 0,
        "timeout escalation must skip gates whose parked snapshot is still unpersisted"
    );
    assert!(
        summary.timeout_actions.is_empty(),
        "unpersisted parked gates must not produce timeout actions"
    );
    assert_eq!(
        summary.reaped_claims, 0,
        "the kept claim must not be reaped during the blocked retry"
    );
    assert!(
        engine.is_park_persist_pending(&first_id),
        "retry must keep tracking the first run while the pending pool is full"
    );
    assert_eq!(
        engine.pending_count_for_sop("s1"),
        1,
        "maintenance retry must not persist the first gate past the pending cap"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the first run's claim remains held until its parked snapshot can persist"
    );
}

#[test]
fn deterministic_start_uses_store_claims() {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let sops = vec![deterministic_sop("det-sop")];
    let mut first = engine_with_sops(sops.clone()).with_store(store.clone());
    let mut second = engine_with_sops(sops).with_store(store);

    first.start_run("det-sop", manual_event()).unwrap();

    assert!(
        second.start_run("det-sop", manual_event()).is_err(),
        "deterministic runs must use the same CAS admission gate"
    );
}

#[test]
fn direct_deterministic_start_cannot_bypass_admission() {
    // start_deterministic_run is public; a DIRECT call must enforce the admission
    // policy itself (not just can_start), so it cannot bypass Hold / Coalesce /
    // the pending-approval pool that start_run enforces.
    let sops = vec![deterministic_sop("det")];
    let mut engine = engine_with_sops(sops);
    engine
        .start_deterministic_run("det", manual_event())
        .unwrap(); // fills the single slot
    assert!(
        engine
            .start_deterministic_run("det", manual_event())
            .is_err(),
        "a direct deterministic start must be declined when admission denies it"
    );
}

#[test]
fn coalesce_resolves_in_flight_run_across_engines() {
    // A2#3: the coalesced run id must come from the SHARED store, so an engine
    // with an empty local map still folds into a sibling engine's in-flight run
    // (Coalesce), not Defer (which would churn AMQP redeliveries).
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.max_concurrent = 1;
    sop.admission_policy = crate::sop::types::SopAdmissionPolicy::Coalesce;
    let sops = vec![sop];
    let mut first = engine_with_sops(sops.clone()).with_store(store.clone());
    let second = engine_with_sops(sops).with_store(store);

    let action = first.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(
        second.active_runs.is_empty(),
        "second engine has no local runs"
    );
    match second.evaluate_admission("s1") {
        SopAdmission::Coalesce { existing_run_id } => assert_eq!(
            existing_run_id, run_id,
            "coalesces into the sibling engine's persisted in-flight run"
        ),
        other => panic!("expected Coalesce across engines, got {other:?}"),
    }
}

#[test]
fn proposals_round_trip_through_engine_store_surface() {
    let engine = SopEngine::new(SopConfig::default());
    let now = now_iso8601();
    let proposal = ProposalRecord {
        id: "prop-1".to_string(),
        kind: ProposalKind::Update,
        status: ProposalStatus::Pending,
        source_run_id: Some("run-1".to_string()),
        sop_name: "s1".to_string(),
        target_content_hash: Some("sha256:abc".to_string()),
        manifest_toml: "[sop]\nname = \"s1\"\ndescription = \"S1\"\n".to_string(),
        procedure_markdown: "## Steps\n\n1. **Do** - It.\n".to_string(),
        provenance: serde_json::json!({"producer": "test"}),
        created_at: now.clone(),
        updated_at: now,
        status_reason: None,
        applied_at: None,
        applied_by: None,
        rollback_path: None,
    };

    engine.save_proposal(&proposal).unwrap();

    assert_eq!(
        engine.load_proposal("prop-1").unwrap().unwrap().sop_name,
        "s1"
    );
    assert_eq!(engine.list_proposals(None).unwrap().len(), 1);
    assert_eq!(
        engine
            .list_proposals(Some(ProposalStatus::Pending))
            .unwrap()
            .len(),
        1
    );
    assert!(
        engine
            .list_proposals(Some(ProposalStatus::Applied))
            .unwrap()
            .is_empty()
    );
}

// ── Cooldown ────────────────────────────────────────

#[test]
fn cooldown_blocks_immediate_restart() {
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.cooldown_secs = 3600; // 1 hour
    let mut engine = engine_with_sops(vec![sop]);

    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    // Complete both steps
    engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "ok".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "ok".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    // Cooldown not elapsed — should block
    assert!(!engine.can_start("s1"));
}

#[test]
fn cooldown_is_shared_across_engine_instances() {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.cooldown_secs = 3600; // 1 hour
    let sops = vec![sop];
    let mut engine_a = engine_with_sops(sops.clone()).with_store(store.clone());
    let mut engine_b = engine_with_sops(sops).with_store(store.clone());

    // Engine A starts and finishes a run (writes a terminal row to the store).
    let action = engine_a.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine_a
        .finish_run(&run_id, SopRunStatus::Completed, None)
        .unwrap();

    // Engine B never ran this SOP, so it has no local finished entry. It must
    // still see the cooldown via the shared store.
    assert!(
        !engine_b.can_start("s1"),
        "a second engine must observe the cooldown from the shared store"
    );
    assert!(
        engine_b.start_run("s1", manual_event()).is_err(),
        "start_run must bail while the shared-store cooldown is active"
    );

    // Advance the stored completion past the cooldown window (supersede the
    // same run's terminal row with an older completed_at, newer revision). The
    // store now reports an elapsed cooldown, so B may start.
    let stored = store.load_run(&run_id).unwrap().unwrap();
    let mut aged = stored.clone();
    aged.revision = stored.revision + 1;
    aged.run.completed_at = Some("2000-01-01T00:00:00Z".to_string());
    store.finish_run(&run_id, &aged).unwrap();

    assert!(
        engine_b.can_start("s1"),
        "once the shared-store cooldown window passes, the second engine may start"
    );
    assert!(
        engine_b.start_run("s1", manual_event()).is_ok(),
        "start_run succeeds after the shared-store cooldown elapses"
    );
}

#[test]
fn restore_runs_keeps_active_and_claims_aligned_over_cap() {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.max_concurrent = 1; // cap of 1, but seed 3 already-running runs
    let now = now_iso8601();
    for i in 0..3 {
        let run = SopRun {
            run_id: format!("restore-{i}"),
            sop_name: "s1".to_string(),
            trigger_event: manual_event(),
            frame_marker_id: format!("marker-{i}"),
            status: SopRunStatus::Running,
            current_step: 1,
            total_steps: 2,
            started_at: now.clone(),
            completed_at: None,
            step_results: Vec::new(),
            waiting_since: None,
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        };
        store
            .save_run(&PersistedRun::new(
                run,
                now.clone(),
                SopTriggerSource::Manual,
            ))
            .unwrap();
    }

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    engine.restore_runs();

    // Every restored run is active...
    assert_eq!(engine.active_runs().len(), 3, "all over-cap runs restored");
    // ...and each has a live store claim (counts == active_runs.len()).
    let (per_sop, total) = store.claim_counts("s1").unwrap();
    assert_eq!(
        total,
        engine.active_runs().len(),
        "every active restored run must hold a live store claim"
    );
    assert_eq!(
        per_sop, 3,
        "all three claims are accounted for under the SOP"
    );
}

// ── Execution modes ─────────────────────────────────

#[test]
fn auto_mode_executes_immediately() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));
}

#[test]
fn supervised_mode_waits_on_first_step() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
}

/// A recorded `deliver` call: `(notice, route, run_id, sop_name, step)`.
type RecordedRouteCall = (
    crate::sop::approval::ApprovalNoticeKind,
    String,
    String,
    String,
    u32,
);

/// A route adapter that records every `deliver` call, so a test can assert the
/// engine fired an out-of-band approval-request notice on park.
#[derive(Default)]
struct RecordingRouteAdapter {
    calls: std::sync::Arc<std::sync::Mutex<Vec<RecordedRouteCall>>>,
}

impl crate::sop::approval::ApprovalRouteAdapter for RecordingRouteAdapter {
    fn deliver(
        &self,
        kind: crate::sop::approval::ApprovalNoticeKind,
        route: &str,
        notice: &crate::sop::approval::GateNotice<'_>,
    ) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push((
            kind,
            route.to_string(),
            notice.run_id.to_string(),
            notice.sop_name.to_string(),
            notice.step,
        ));
        Ok(())
    }
}

fn policied_supervised_engine(
    request_route: Option<&str>,
    adapter: std::sync::Arc<dyn crate::sop::approval::ApprovalRouteAdapter>,
) -> SopEngine {
    use zeroclaw_config::schema::ApprovalPolicyConfig;
    let mut config = SopConfig::default();
    config.approval.policies.insert(
        "prod".to_string(),
        ApprovalPolicyConfig {
            required_group: None,
            quorum: 0,
            request_route: request_route.map(String::from),
            escalation_route: None,
        },
    );
    // A supervised SOP whose first step names the `prod` policy, so starting it
    // parks at a policied approval gate.
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.steps[0].policy = Some("prod".to_string());
    engine_with_config_sops(config, vec![sop]).with_approval_broker(std::sync::Arc::new(
        crate::sop::approval::ApprovalBroker::with_route(adapter),
    ))
}

#[test]
fn parking_at_a_policied_gate_delivers_the_request_route() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    let mut engine = policied_supervised_engine(Some("discord.ops:123456789"), adapter);

    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(
        matches!(action, SopRunAction::WaitApproval { .. }),
        "supervised policied step parks for approval"
    );

    let recorded = calls.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        1,
        "exactly one out-of-band request-route delivery fired on park"
    );
    let (notice, route, delivered_run, sop_name, step) = &recorded[0];
    assert_eq!(
        *notice,
        crate::sop::approval::ApprovalNoticeKind::Request,
        "parking sends the initial request notice"
    );
    assert_eq!(route, "discord.ops:123456789", "the policy's request_route");
    assert_eq!(delivered_run, &run_id, "carries the parked run id");
    assert_eq!(sop_name, "s1", "carries the SOP name");
    assert_eq!(*step, 1, "carries the parked step number");
}

#[test]
fn park_withholds_the_request_route_until_the_snapshot_is_durable() {
    // A route notice must NOT fire for a gate whose parked snapshot is not yet
    // durable: when save_run fails at park, the exec claim is kept (fail-closed) and
    // the request-route delivery is withheld (retry_pending_park_persists re-issues
    // it once a retry persists the park).
    use zeroclaw_config::schema::ApprovalPolicyConfig;
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    let mut config = SopConfig::default();
    config.approval.policies.insert(
        "prod".to_string(),
        ApprovalPolicyConfig {
            required_group: None,
            quorum: 0,
            request_route: Some("discord.ops:1".to_string()),
            escalation_route: None,
        },
    );
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.steps[0].policy = Some("prod".to_string());
    let store = std::sync::Arc::new(FailingSaveStore {
        inner: InMemoryRunStore::new(),
    });
    let mut engine = engine_with_config_sops(config, vec![sop])
        .with_approval_broker(std::sync::Arc::new(
            crate::sop::approval::ApprovalBroker::with_route(adapter),
        ))
        .with_store(store);

    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(
        matches!(
            action,
            SopRunAction::Pending { ref reason, .. }
                if reason.contains("park snapshot not yet durably persisted")
        ),
        "the supervised policied step reports durable-park backpressure"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "no request-route delivery may fire while the parked snapshot is not durable"
    );
    assert!(
        engine.is_park_persist_pending(&run_id),
        "the run is tracked for a park-persist retry (claim kept, fail-closed)"
    );
}

#[test]
fn parking_at_a_policied_gate_without_a_request_route_delivers_nothing() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    // Same policied gate, but the policy names NO request_route.
    let mut engine = policied_supervised_engine(None, adapter);

    let action = engine.start_run("s1", manual_event()).unwrap();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
    assert!(
        calls.lock().unwrap().is_empty(),
        "no request_route configured means no out-of-band delivery"
    );
}

#[test]
fn step_by_step_waits_on_every_step() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::StepByStep,
        SopPriority::Normal,
    )]);

    // Step 1: WaitApproval
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));

    // Approve step 1
    let action = approve_gate_cli(&mut engine, &run_id);
    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));

    // Complete step 1, step 2 should also WaitApproval
    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "ok".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
}

#[test]
fn priority_based_critical_gates() {
    // [SEC-FLIP] Critical/High under PriorityBased now GATE (was auto-execute).
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::PriorityBased,
        SopPriority::Critical,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    assert!(
        matches!(action, SopRunAction::WaitApproval { .. }),
        "critical PriorityBased SOPs must gate, not auto-run"
    );
}

#[test]
fn priority_based_normal_supervised() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::PriorityBased,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    // Normal + PriorityBased → Supervised → WaitApproval on step 1
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
}

#[test]
fn requires_confirmation_overrides_auto() {
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Critical);
    sop.steps[0].requires_confirmation = true;
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    // Even in Auto mode, requires_confirmation forces WaitApproval
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
}

#[test]
fn step_mode_can_tighten_auto_step() {
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps[0].mode = Some(SopExecutionMode::StepByStep);
    let mut engine = engine_with_sops(vec![sop]);

    let action = engine.start_run("s1", manual_event()).unwrap();

    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
}

#[test]
fn step_mode_cannot_relax_step_by_step_step() {
    let mut sop = test_sop("s1", SopExecutionMode::StepByStep, SopPriority::Normal);
    sop.steps[0].mode = Some(SopExecutionMode::Auto);
    let mut engine = engine_with_sops(vec![sop]);

    let action = engine.start_run("s1", manual_event()).unwrap();

    assert!(
        matches!(action, SopRunAction::WaitApproval { .. }),
        "a step auto override must not relax the SOP's step_by_step gate, got {action:?}"
    );
}

#[test]
fn out_of_band_required_prevents_step_auto_relaxing_gate() {
    let mut sop = test_sop("s1", SopExecutionMode::StepByStep, SopPriority::Normal);
    sop.steps[0].mode = Some(SopExecutionMode::Auto);
    let mut engine = engine_with_config_sops(
        SopConfig {
            approval_mode: zeroclaw_config::schema::ApprovalMode::OutOfBandRequired,
            ..SopConfig::default()
        },
        vec![sop],
    );

    let action = engine.start_run("s1", manual_event()).unwrap();

    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
}

// ── Approve ─────────────────────────────────────────

#[test]
fn approve_transitions_to_execute() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Run should be WaitingApproval
    let run = engine.active_runs().get(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::WaitingApproval);

    // Approve
    let action = approve_gate_cli(&mut engine, &run_id);
    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));

    let run = engine.active_runs().get(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::Running);
}

#[test]
fn approve_non_waiting_fails() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(engine.approve_step(&run_id).is_err());
}

#[test]
fn step_auto_override_cannot_defeat_supervised_step_one_gate() {
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.steps[0].mode = Some(SopExecutionMode::Auto);
    let mut engine = engine_with_sops(vec![sop]);

    let action = engine.start_run("s1", manual_event()).unwrap();
    assert!(
        matches!(action, SopRunAction::WaitApproval { .. }),
        "supervised SOP must gate step 1 even when the step overrides mode to auto, got {action:?}"
    );
    let run_id = extract_run_id(&action).to_string();
    assert_eq!(
        engine.active_runs().get(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the run must park at the gate, not sit Running at step 1"
    );
}

// ── Advance step gate guard ─────────────────────────────
//
// A driver calling `sop_advance` while a run is parked at an external
// gate (WaitingApproval or PausedCheckpoint) used to be allowed to
// fabricate a Completed step result, record it, and dispatch the next
// step — silently bypassing the approval flow or the deterministic
// checkpoint resume. `advance_step` now refuses those calls.

#[test]
fn advance_step_rejects_waiting_approval_run() {
    // requires_confirmation forces the run to WaitApproval on step 1.
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Critical);
    sop.steps[0].requires_confirmation = true;
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Sanity: run is parked at the gate.
    let run = engine.active_runs().get(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::WaitingApproval);
    let step_results_before = run.step_results.len();

    // Driver tries to fabricate success for the gated step.
    let err = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "fabricated".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap_err();

    // Error must point at the gate, not the run id.
    assert!(
        err.to_string().contains("WaitingApproval"),
        "rejection should mention the gate status, got: {err}"
    );

    // The run state must be unchanged: still WaitingApproval, no
    // phantom step result recorded, no next step dispatched.
    let run = engine.active_runs().get(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::WaitingApproval);
    assert_eq!(run.step_results.len(), step_results_before);
}

#[test]
fn advance_step_rejects_paused_checkpoint_run() {
    // A deterministic SOP with a Checkpoint step pauses the run in
    // PausedCheckpoint after step 1 completes. Driving `sop_advance`
    // directly must be rejected — the only legitimate resume path is
    // `approve_step`.
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]);
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Advance through step 1 (Execute) to reach the checkpoint.
    engine
        .advance_deterministic_step(&run_id, serde_json::json!({"ok": true}), None)
        .unwrap();
    let run = engine.get_run(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::PausedCheckpoint);

    // Driver tries to fabricate completion of the checkpoint step.
    let err = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "fabricated".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap_err();

    assert!(
        err.to_string().contains("PausedCheckpoint"),
        "rejection should mention the gate status, got: {err}"
    );

    // The run must still be parked at the checkpoint, not advanced
    // past it.
    let run = engine.get_run(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::PausedCheckpoint);
}

#[test]
fn advance_step_still_works_for_running_run() {
    // Control case: a non-paused run must still be drivable through
    // sop_advance. Without this case, the new guard could be hiding
    // a regression on the happy path.
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "done".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    assert!(matches!(action, SopRunAction::ExecuteStep { .. }));
}

// ── Context formatting ──────────────────────────────

#[test]
fn step_context_includes_sop_name_and_step() {
    let sop = test_sop(
        "pump-shutdown",
        SopExecutionMode::Auto,
        SopPriority::Critical,
    );
    let run = SopRun {
        run_id: "run-001".into(),
        sop_name: "pump-shutdown".into(),
        trigger_event: manual_event(),
        frame_marker_id: "marker-001".into(),
        status: SopRunStatus::Running,
        current_step: 1,
        total_steps: 2,
        started_at: now_iso8601(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: None,
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    let ctx = format_step_context(&sop, &run, &sop.steps[0], &SopConfig::default());
    assert!(ctx.contains("pump-shutdown"));
    assert!(ctx.contains("Step 1 of 2"));
    assert!(ctx.contains("Step one"));
}

// ── Get run (active + finished) ─────────────────────

#[test]
fn get_run_finds_active_and_finished() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Active
    assert!(engine.get_run(&run_id).is_some());
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Running
    );

    // Complete
    engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "ok".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "ok".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    // Now finished — still findable
    assert!(engine.get_run(&run_id).is_some());
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Completed
    );

    // Unknown
    assert!(engine.get_run("nonexistent").is_none());
}

// ── ISO-8601 helpers ────────────────────────────────

#[test]
fn iso8601_roundtrip() {
    let ts = now_iso8601();
    let secs = parse_iso8601_secs(&ts);
    assert!(secs.is_some());
    // Should be close to current time
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now.abs_diff(secs.unwrap()) < 2);
}

#[test]
fn parse_known_timestamp() {
    // 2026-01-01T00:00:00Z
    let secs = parse_iso8601_secs("2026-01-01T00:00:00Z").unwrap();
    // Jan 1 2026 = 20454 days since epoch * 86400
    assert_eq!(secs, 20454 * 86400);
}

// ── Approval timeout ─────────────────────────────────

#[test]
fn timeout_escalates_critical_no_self_approve() {
    // [SEC-FLIP] Under the default fail-closed Escalate, a Critical/High SOP
    // that times out is NO LONGER auto-approved: it stays WaitingApproval and a
    // gate_escalated ledger row is recorded. (Was: timeout_auto_approves_critical.)
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        ..SopConfig::default()
    });
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Critical,
    )]);

    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));

    let run = engine.active_runs.get_mut(&run_id).unwrap();
    run.waiting_since = Some("2020-01-01T00:00:00Z".into());

    let actions = engine.check_approval_timeouts();
    assert!(actions.is_empty(), "escalate produces no resumed action");
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "critical run stays gated under fail-closed escalate"
    );
    assert!(
        engine
            .run_events(&run_id)
            .unwrap()
            .iter()
            .any(|ev| ev.kind == "gate_escalated"),
        "escalation is recorded in the ledger"
    );
}

#[test]
fn timeout_escalation_without_distinct_route_resurfaces_request_route() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    let mut engine = policied_supervised_engine(Some("discord.ops:123456789"), adapter);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
    calls.lock().unwrap().clear();

    crate::sop::approval::timeout::apply_timeout_action(
        &mut engine,
        &run_id,
        zeroclaw_config::schema::ApprovalTimeoutAction::Escalate,
    );

    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [(
            crate::sop::approval::ApprovalNoticeKind::Escalation,
            "discord.ops:123456789".to_string(),
            run_id,
            "s1".to_string(),
            1
        )],
        "an unset escalation_route must re-surface the gate to request_route"
    );
}

#[test]
fn maintenance_tick_fires_fail_closed_timeout() {
    // EPIC A1: the daemon tick drives check_approval_timeouts. An overdue gate
    // under the default fail-closed Escalate stays WaitingApproval (no
    // self-approve) and the escalation is recorded; the summary counts it.
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        ..SopConfig::default()
    });
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    // Force the gate overdue.
    engine.active_runs.get_mut(&run_id).unwrap().waiting_since =
        Some("2020-01-01T00:00:00Z".into());

    let summary = engine.run_maintenance_tick();

    assert!(
        !summary.is_empty(),
        "an overdue gate makes the pass non-empty"
    );
    assert_eq!(summary.timed_out, 1, "the overdue gate timed out");
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "fail-closed escalate keeps the gate open, never self-approves"
    );
    assert!(
        engine
            .run_events(&run_id)
            .unwrap()
            .iter()
            .any(|ev| ev.kind == "gate_escalated"),
        "the tick recorded the escalation in the ledger"
    );
}

#[test]
fn maintenance_tick_is_a_noop_when_nothing_is_due() {
    let mut engine = SopEngine::new(SopConfig::default());
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    // No runs started -> nothing to time out, reap, or prune.
    let summary = engine.run_maintenance_tick();
    assert!(summary.is_empty(), "a quiet tick is a no-op");
    assert_eq!(summary.timed_out, 0);
    assert_eq!(summary.reaped_claims, 0);
    assert_eq!(summary.pruned_runs, 0);
}

#[test]
fn timeout_cancel_finishes_run() {
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        approval_timeout_action: zeroclaw_config::schema::ApprovalTimeoutAction::Cancel,
        ..SopConfig::default()
    });
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine.active_runs.get_mut(&run_id).unwrap().waiting_since =
        Some("2020-01-01T00:00:00Z".into());

    let actions = engine.check_approval_timeouts();
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], SopRunAction::Completed { .. }));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Cancelled,
        "cancel terminates the run (retained as a terminal record)"
    );
}

#[test]
fn timeout_cancel_terminal_failure_does_not_write_timeout_event() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        approval_timeout_action: zeroclaw_config::schema::ApprovalTimeoutAction::Cancel,
        ..SopConfig::default()
    })
    .with_store(store.clone());
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine.active_runs.get_mut(&run_id).unwrap().waiting_since =
        Some("2020-01-01T00:00:00Z".into());

    store
        .fail_finish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let actions = engine.check_approval_timeouts();

    assert!(
        actions.is_empty(),
        "failed cancel persistence retries later"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the gate stays waiting when terminal persistence fails"
    );
    assert!(
        !engine
            .run_events(&run_id)
            .unwrap()
            .iter()
            .any(|ev| ev.kind == "gate_timed_out"),
        "timeout cancel must not write a ledger row without terminal state"
    );
}

#[test]
fn timeout_escalate_save_failure_does_not_write_escalation_event() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        ..SopConfig::default()
    })
    .with_store(store.clone());
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    let overdue = "2020-01-01T00:00:00Z".to_string();
    engine.active_runs.get_mut(&run_id).unwrap().waiting_since = Some(overdue.clone());

    store
        .fail_save
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let actions = engine.check_approval_timeouts();

    assert!(
        actions.is_empty(),
        "failed escalation persistence retries later"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the gate stays waiting when restamp persistence fails"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().waiting_since.as_deref(),
        Some(overdue.as_str()),
        "failed escalation persistence rolls back the in-memory restamp"
    );
    assert!(
        !engine
            .run_events(&run_id)
            .unwrap()
            .iter()
            .any(|ev| ev.kind == "gate_escalated"),
        "timeout escalate must not write a ledger row without the restamp"
    );
}

#[test]
fn timeout_auto_approve_legacy_resumes() {
    // The legacy fail-open behavior is reachable ONLY via the explicit opt-in.
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        approval_timeout_action: zeroclaw_config::schema::ApprovalTimeoutAction::AutoApprove,
        ..SopConfig::default()
    });
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Critical,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine.active_runs.get_mut(&run_id).unwrap().waiting_since =
        Some("2020-01-01T00:00:00Z".into());

    let actions = engine.check_approval_timeouts();
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], SopRunAction::ExecuteStep { .. }));
}

#[test]
fn escalate_never_self_approves_any_priority() {
    // [SEC-FLIP] guard: under the default action, NO priority auto-approves.
    for priority in [
        SopPriority::Critical,
        SopPriority::High,
        SopPriority::Normal,
        SopPriority::Low,
    ] {
        let mut engine = SopEngine::new(SopConfig {
            approval_timeout_secs: 1,
            ..SopConfig::default()
        });
        engine.set_sops_for_test(vec![test_sop("s1", SopExecutionMode::Supervised, priority)]);
        let action = engine.start_run("s1", manual_event()).unwrap();
        let run_id = extract_run_id(&action).to_string();
        engine.active_runs.get_mut(&run_id).unwrap().waiting_since =
            Some("2020-01-01T00:00:00Z".into());

        let actions = engine.check_approval_timeouts();
        assert!(
            actions.is_empty(),
            "priority {priority:?} must not self-approve under fail-closed default"
        );
        assert_eq!(
            engine.get_run(&run_id).unwrap().status,
            SopRunStatus::WaitingApproval
        );
    }
}

#[test]
fn timeout_does_not_auto_approve_normal() {
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 1,
        ..SopConfig::default()
    });
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);

    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Backdate waiting_since
    let run = engine.active_runs.get_mut(&run_id).unwrap();
    run.waiting_since = Some("2020-01-01T00:00:00Z".into());

    // Normal priority → no auto-approve
    let actions = engine.check_approval_timeouts();
    assert!(actions.is_empty());
    // Run should still be WaitingApproval
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval
    );
}

#[test]
fn timeout_zero_disables_check() {
    let mut engine = SopEngine::new(SopConfig {
        approval_timeout_secs: 0,
        ..SopConfig::default()
    });
    engine.set_sops_for_test(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Critical,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let run = engine.active_runs.get_mut(&run_id).unwrap();
    run.waiting_since = Some("2020-01-01T00:00:00Z".into());

    let actions = engine.check_approval_timeouts();
    assert!(actions.is_empty());
}

#[test]
fn waiting_since_set_on_wait_approval() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let run = engine.get_run(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::WaitingApproval);
    assert!(run.waiting_since.is_some());
}

// ── A1: HITL admission (parked runs release their exec slot) ──────

#[test]
fn parked_approval_run_releases_exec_slot() {
    // A run parked at a HITL approval must release its exec claim so a second
    // trigger for the same SOP (max_concurrent = 1) is admitted, not dropped.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let a1 = engine.start_run("s1", manual_event()).unwrap();
    let run1 = extract_run_id(&a1).to_string();
    assert_eq!(
        engine.get_run(&run1).unwrap().status,
        SopRunStatus::WaitingApproval
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "a parked approval run must not hold an exec claim"
    );
    assert!(
        engine.can_start("s1"),
        "the freed slot admits the next trigger"
    );

    // Second trigger admits (pre-A1 this was dropped on concurrency) and parks too.
    let a2 = engine.start_run("s1", manual_event()).unwrap();
    let run2 = extract_run_id(&a2).to_string();
    assert_ne!(run1, run2);
    assert_eq!(
        engine.get_run(&run2).unwrap().status,
        SopRunStatus::WaitingApproval
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "both parked runs hold no exec claim"
    );
}

#[test]
fn resume_reacquires_exec_slot() {
    // Approving a parked run re-establishes its exec claim so it counts against
    // concurrency again while it finishes executing.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let a = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&a).to_string();
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "parked before approval: no exec claim"
    );

    let _ = approve_gate_cli(&mut engine, &run_id);
    assert_eq!(
        store.claim_counts("s1").unwrap().1,
        1,
        "an approved+resumed run re-acquires its exec claim"
    );
}

#[test]
fn resume_admission_enforces_per_sop_concurrency_cap() {
    // Reviewer scenario: with `max_concurrent = 1` and the default unbounded pending
    // pool, many runs can park (each releasing its slot), then approving them all must
    // NOT let them all resume at once. Capped resume: the first resumes; the rest are
    // refused at capacity (`DeferredAtCapacity`) and stay parked, re-resolvable.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    // Two runs park in sequence (the first frees its slot on park, so the second admits).
    let a = engine.start_run("s1", manual_event()).unwrap();
    let id_a = extract_run_id(&a).to_string();
    assert!(
        matches!(a, SopRunAction::WaitApproval { .. }),
        "run A parks: {a:?}"
    );
    let b = engine.start_run("s1", manual_event()).unwrap();
    let id_b = extract_run_id(&b).to_string();
    assert!(
        matches!(b, SopRunAction::WaitApproval { .. }),
        "run B parks too: {b:?}"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "both parked: no exec claim held"
    );

    // Approve A: it resumes into the single free slot.
    let out_a = engine
        .resolve_gate(
            &id_a,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(out_a.is_resumed(), "A resumes: {out_a:?}");
    assert_eq!(
        store.claim_counts("s1").unwrap().0,
        1,
        "A holds the one exec slot"
    );

    // Approve B: the slot is taken, so B must defer at capacity - never oversubscribe.
    let out_b = engine
        .resolve_gate(
            &id_b,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(
        matches!(out_b, ResolveOutcome::DeferredAtCapacity),
        "B is refused at capacity, not oversubscribed: {out_b:?}"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap().0,
        1,
        "still exactly one exec slot in use, not two"
    );
    assert!(
        matches!(engine.gate_state(&id_b), GateState::Waiting { .. }),
        "B stays WaitingApproval, re-resolvable"
    );
}

#[test]
fn resume_admission_enforces_global_concurrency_cap() {
    // The global `max_concurrent_total` is enforced on resume too: two DIFFERENT SOPs
    // (each `max_concurrent = 1`) share a global cap of 1. Both park; approving both
    // resumes only the first - the second defers at capacity.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut s1 = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    s1.max_concurrent = 1;
    let mut s2 = test_sop("s2", SopExecutionMode::Supervised, SopPriority::Normal);
    s2.max_concurrent = 1;
    let cfg = SopConfig {
        max_concurrent_total: 1,
        ..SopConfig::default()
    };
    let mut engine = engine_with_config_sops(cfg, vec![s1, s2]).with_store(store.clone());

    let a = engine.start_run("s1", manual_event()).unwrap();
    let id_a = extract_run_id(&a).to_string();
    let b = engine.start_run("s2", manual_event()).unwrap();
    let id_b = extract_run_id(&b).to_string();
    assert!(
        matches!(a, SopRunAction::WaitApproval { .. })
            && matches!(b, SopRunAction::WaitApproval { .. }),
        "both runs park for approval"
    );

    let out_a = engine
        .resolve_gate(
            &id_a,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(
        out_a.is_resumed(),
        "the first resumes into the one global slot"
    );
    let out_b = engine
        .resolve_gate(
            &id_b,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(
        matches!(out_b, ResolveOutcome::DeferredAtCapacity),
        "the global cap refuses the second resume: {out_b:?}"
    );
    assert_eq!(
        store.claim_counts("s2").unwrap().1,
        1,
        "exactly one exec slot in use globally, not two"
    );
}

#[test]
fn checkpoint_resume_enforces_concurrency_cap() {
    // The cap applies to the checkpoint-resume path (`approve_step`) too, via the same
    // reacquire chokepoint. Two deterministic runs park at a checkpoint (each frees its
    // slot); approving both resumes only the first - the second is refused at capacity
    // with the typed backpressure marker, and stays paused.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = deterministic_sop("det-cp");
    sop.max_concurrent = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let a = engine.start_run("det-cp", manual_event()).unwrap();
    let id_a = extract_run_id(&a).to_string();
    engine
        .advance_deterministic_step(&id_a, serde_json::json!("a1"), None)
        .unwrap();
    assert_eq!(
        engine.get_run(&id_a).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );
    let b = engine.start_run("det-cp", manual_event()).unwrap();
    let id_b = extract_run_id(&b).to_string();
    engine
        .advance_deterministic_step(&id_b, serde_json::json!("b1"), None)
        .unwrap();
    assert_eq!(
        engine.get_run(&id_b).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (0, 0),
        "both parked at the checkpoint: no exec claim held"
    );

    engine.approve_step(&id_a).unwrap();
    assert_eq!(
        store.claim_counts("det-cp").unwrap().0,
        1,
        "A holds the one slot after resuming"
    );

    let err = engine
        .approve_step(&id_b)
        .expect_err("B's checkpoint resume must be refused at capacity");
    assert!(
        err_is_resume_at_capacity(&err),
        "the refusal is typed capacity backpressure, not a fault: {err}"
    );
    assert_eq!(
        engine.get_run(&id_b).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "B stays paused at the checkpoint, re-resolvable"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap().0,
        1,
        "still exactly one slot in use, not two"
    );
}

#[test]
fn sqlite_daemon_restart_resumes_parked_run_and_enforces_cap() {
    // Near-live boundary evidence: with a REAL file-backed SQLite store, runs parked for
    // approval survive a daemon "restart" (a fresh engine over the same DB), restore
    // holding no exec slot, and the resume concurrency cap holds ACROSS the restart -
    // exercising the durable status round-trip plus capped `reacquire_claim_on_resume`.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sop.db");

    // Boot 1: park two runs of a max_concurrent=1 SOP, then shut down.
    let (id_a, id_b);
    {
        let store =
            std::sync::Arc::new(crate::sop::store::sqlite::SqliteRunStore::open(&db).unwrap());
        let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
        sop.max_concurrent = 1;
        let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
        let a = engine.start_run("s1", manual_event()).unwrap();
        id_a = extract_run_id(&a).to_string();
        let b = engine.start_run("s1", manual_event()).unwrap();
        id_b = extract_run_id(&b).to_string();
        assert!(
            matches!(a, SopRunAction::WaitApproval { .. })
                && matches!(b, SopRunAction::WaitApproval { .. }),
            "both runs park for approval"
        );
        assert_eq!(
            store.claim_counts("s1").unwrap(),
            (0, 0),
            "parked runs hold no exec slot (durably)"
        );
    }

    // Boot 2: restart over the SAME DB, restore, then approve both.
    {
        let store =
            std::sync::Arc::new(crate::sop::store::sqlite::SqliteRunStore::open(&db).unwrap());
        let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
        sop.max_concurrent = 1;
        let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
        engine.restore_runs();
        assert_eq!(
            engine.get_run(&id_a).map(|r| r.status),
            Some(SopRunStatus::WaitingApproval),
            "run A restored WaitingApproval after restart"
        );
        assert_eq!(
            engine.get_run(&id_b).map(|r| r.status),
            Some(SopRunStatus::WaitingApproval),
            "run B restored WaitingApproval after restart"
        );
        assert_eq!(
            store.claim_counts("s1").unwrap(),
            (0, 0),
            "restored parked runs hold no exec claim"
        );

        // Approve A: resumes into the free slot. Approve B: refused at capacity - the cap
        // holds across the restart boundary.
        let out_a = engine
            .resolve_gate(
                &id_a,
                ApprovalDecision::Approve,
                ApprovalPrincipal::cli(None),
            )
            .unwrap();
        assert!(out_a.is_resumed(), "A resumes after restart: {out_a:?}");
        let out_b = engine
            .resolve_gate(
                &id_b,
                ApprovalDecision::Approve,
                ApprovalPrincipal::cli(None),
            )
            .unwrap();
        assert!(
            matches!(out_b, ResolveOutcome::DeferredAtCapacity),
            "the resume cap holds across restart: B is refused at capacity: {out_b:?}"
        );
        assert_eq!(
            store.claim_counts("s1").unwrap().0,
            1,
            "exactly one exec slot in use after restart + resume, not two"
        );
    }
}

#[test]
fn rollback_activated_run_durably_cancels_a_parked_sibling() {
    // 2b atomic-rollback: a sibling that PARKED (persisted) during activation and is then
    // rolled back (because a later sibling failed to activate) must be durably CANCELLED,
    // not merely dropped in memory - otherwise `restore_runs` reconstructs an orphaned
    // parked run after a restart, duplicating a delivery that was deferred + requeued.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )])
    .with_store(store.clone());
    // A sibling that activated and PARKED at its step-1 approval gate (persisted).
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(matches!(action, SopRunAction::WaitApproval { .. }));
    assert!(
        store
            .load_active_runs()
            .unwrap()
            .iter()
            .any(|r| r.run.run_id == run_id),
        "the parked sibling is durable before rollback"
    );

    // Roll it back, as the atomic batch does when a later sibling's activation fails.
    engine.rollback_activated_run(&run_id);
    assert!(
        engine.get_run(&run_id).is_none(),
        "the rolled-back sibling is dropped in memory"
    );
    // The durable row is now terminal Cancelled, not an active parked run.
    assert!(
        store
            .load_active_runs()
            .unwrap()
            .iter()
            .all(|r| r.run.run_id != run_id),
        "the rolled-back parked sibling is no longer a durable ACTIVE run"
    );

    // A restart must NOT resurrect it as a LIVE parked run (the post-requeue duplicate);
    // at most it appears as terminal history.
    let mut fresh = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )])
    .with_store(store.clone());
    fresh.restore_runs();
    let restored = fresh.get_run(&run_id).map(|r| r.status);
    assert!(
        restored.is_none() || restored == Some(SopRunStatus::Cancelled),
        "restart must not resurrect the rolled-back sibling as a live parked run (got {restored:?})"
    );
}

#[test]
fn restored_parked_run_holds_no_exec_claim() {
    // A parked run persisted before a restart must restore WITHOUT re-taking an
    // exec slot (it is waiting on a human, not executing), so the slot stays free
    // for a fresh trigger (max_concurrent = 1).
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 1;
    let now = now_iso8601();
    let parked = SopRun {
        run_id: "parked-1".to_string(),
        sop_name: "s1".to_string(),
        trigger_event: manual_event(),
        frame_marker_id: "marker".to_string(),
        status: SopRunStatus::WaitingApproval,
        current_step: 1,
        total_steps: 2,
        started_at: now.clone(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: Some(now.clone()),
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    store
        .save_run(&PersistedRun::new(
            parked,
            now.clone(),
            SopTriggerSource::Manual,
        ))
        .unwrap();

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    engine.restore_runs();

    assert_eq!(
        engine.get_run("parked-1").unwrap().status,
        SopRunStatus::WaitingApproval,
        "the parked run is restored"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "a restored parked run holds no exec claim"
    );
    assert!(
        engine.can_start("s1"),
        "its slot stays free for a new trigger"
    );
}

#[test]
fn restore_fails_closed_when_retention_inspection_errors() {
    // Finding 3: if inspecting the terminal-rollback retention marker ERRORS during
    // restore, we must fail CLOSED and KEEP the claim - a transient read failure must
    // not discard a claim the marker exists to preserve. (The prior code mapped the
    // error to `retained = false`, routing a legitimate marker into the release branch.)
    let store = std::sync::Arc::new(FailingSaveLeasedStore::healthy());
    // Seed a parked run whose current step has NO recorded result (a legitimate,
    // non-stale terminal-rollback marker) plus a retained claim for it.
    let now = now_iso8601();
    let parked = SopRun {
        run_id: "parked-1".to_string(),
        sop_name: "s1".to_string(),
        trigger_event: manual_event(),
        frame_marker_id: "marker".to_string(),
        status: SopRunStatus::WaitingApproval,
        current_step: 1,
        total_steps: 2,
        started_at: now.clone(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: Some(now.clone()),
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    store
        .save_run(&PersistedRun::new(parked, now, SopTriggerSource::Manual))
        .unwrap();
    store.try_claim_run("parked-1", "s1", 1, 4).unwrap();
    store
        .mark_claim_retained_after_terminal_rollback("parked-1")
        .unwrap();
    assert_eq!(
        store.claim_counts("s1").unwrap().1,
        1,
        "seeded a retained terminal-rollback claim"
    );

    // Make the retention inspection fail during restore.
    store.set_fail_has_retained(true);
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    engine.restore_runs();

    // Fail-closed: the claim is PRESERVED, not discarded, and the run is still restored.
    assert_eq!(
        store.claim_counts("s1").unwrap().1,
        1,
        "an inspection error must fail closed: the retained claim survives (not released)"
    );
    assert!(
        engine.get_run("parked-1").is_some(),
        "the parked run is still restored"
    );
}

#[test]
fn restore_releases_stale_claim_for_parked_run() {
    // A durable store written before this change can carry a parked run PLUS a
    // live claim row. restore_runs must RELEASE that stale claim so the run does
    // not keep blocking a same-SOP admission (nor get its lease extended forever).
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 1;
    // Seed a live claim for the parked run (the old behavior kept it).
    assert!(
        store
            .try_claim_run("parked-1", "s1", 1, 4)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "seeded a stale claim"
    );
    let now = now_iso8601();
    let parked = SopRun {
        run_id: "parked-1".to_string(),
        sop_name: "s1".to_string(),
        trigger_event: manual_event(),
        frame_marker_id: "marker".to_string(),
        status: SopRunStatus::WaitingApproval,
        current_step: 1,
        total_steps: 2,
        started_at: now.clone(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: Some(now.clone()),
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    store
        .save_run(&PersistedRun::new(
            parked,
            now.clone(),
            SopTriggerSource::Manual,
        ))
        .unwrap();

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    engine.restore_runs();

    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "restore must release the parked run's stale claim"
    );
    assert!(
        engine.can_start("s1"),
        "the freed slot admits a new trigger after restart"
    );
}

/// Delegates to an in-memory store but can be flipped to fail claim acquisition
/// (both the capped `try_claim_run` the resume reacquire now uses and the uncapped
/// `renew_claim_for_restore`), to prove resume fails CLOSED when the claim store
/// errors. Flipped ON only after the initial admit so `start_run` still succeeds.
struct FailingReacquireStore {
    inner: InMemoryRunStore,
    fail_claim: std::sync::atomic::AtomicBool,
}
impl SopRunStore for FailingReacquireStore {
    fn save_run(&self, r: &PersistedRun) -> Result<(), StoreError> {
        self.inner.save_run(r)
    }
    fn save_run_with_event(&self, r: &PersistedRun, e: &SopEventRecord) -> Result<u64, StoreError> {
        self.inner.save_run_with_event(r, e)
    }
    fn finish_run(&self, id: &str, t: &PersistedRun) -> Result<(), StoreError> {
        self.inner.finish_run(id, t)
    }
    fn finish_run_with_event(
        &self,
        id: &str,
        t: &PersistedRun,
        e: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        self.inner.finish_run_with_event(id, t, e)
    }
    fn load_terminal_runs(
        &self,
        _limit: usize,
    ) -> Result<Vec<crate::sop::store::PersistedRun>, crate::sop::store::StoreError> {
        Ok(Vec::new())
    }
    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        self.inner.load_active_runs()
    }
    fn load_run(&self, id: &str) -> Result<Option<PersistedRun>, StoreError> {
        self.inner.load_run(id)
    }
    fn last_terminal_completed_at(&self, s: &str) -> Result<Option<String>, StoreError> {
        self.inner.last_terminal_completed_at(s)
    }
    fn try_claim_run(
        &self,
        id: &str,
        s: &str,
        p: usize,
        g: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        if self.fail_claim.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected claim failure".into()));
        }
        self.inner.try_claim_run(id, s, p, g)
    }
    fn renew_claim_for_restore(&self, id: &str, s: &str) -> Result<ClaimToken, StoreError> {
        if self.fail_claim.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected renew failure".into()));
        }
        self.inner.renew_claim_for_restore(id, s)
    }
    fn claim_counts(&self, s: &str) -> Result<(usize, usize), StoreError> {
        self.inner.claim_counts(s)
    }
    fn heartbeat_claim(&self, t: &ClaimToken) -> Result<(), StoreError> {
        self.inner.heartbeat_claim(t)
    }
    fn release_claim(&self, t: &ClaimToken) -> Result<(), StoreError> {
        self.inner.release_claim(t)
    }
    fn expired_claims(&self, n: &str) -> Result<Vec<ClaimToken>, StoreError> {
        self.inner.expired_claims(n)
    }
    fn append_event(&self, e: &SopEventRecord) -> Result<u64, StoreError> {
        self.inner.append_event(e)
    }
    fn list_events(&self, id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        self.inner.list_events(id)
    }
    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError> {
        self.inner.save_proposal(p)
    }
    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        self.inner.load_proposal(id)
    }
    fn list_proposals(&self, s: Option<ProposalStatus>) -> Result<Vec<ProposalRecord>, StoreError> {
        self.inner.list_proposals(s)
    }
    fn prune(&self, p: &RetentionPolicy) -> Result<usize, StoreError> {
        self.inner.prune(p)
    }
    fn health_check(&self) -> bool {
        self.inner.health_check()
    }
    fn backend(&self) -> &'static str {
        "failing-reacquire-test"
    }
}

#[test]
fn resume_fails_closed_when_claim_reacquire_fails() {
    // If the claim store errors during resume, the run must NOT execute
    // uncounted: the resume aborts (Err) and the gate stays WaitingApproval.
    let store = std::sync::Arc::new(FailingReacquireStore {
        inner: InMemoryRunStore::new(),
        fail_claim: std::sync::atomic::AtomicBool::new(false),
    });
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let a = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&a).to_string();
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval
    );
    // Fail the claim store now (after the admit): the resume reacquire hits a
    // store fault (not capacity backpressure) and must abort fail-closed.
    store
        .fail_claim
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let res = engine.resolve_gate(
        &run_id,
        ApprovalDecision::Approve,
        ApprovalPrincipal::cli(None),
    );
    assert!(
        res.is_err(),
        "resume must abort when the exec claim cannot be re-acquired"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the gate must stay WaitingApproval (re-resolvable), not execute uncounted"
    );
    // A1#2: the claim is secured BEFORE the audit row, so a reacquire failure
    // must leave NO false `gate_resolved` approval row in the ledger (which
    // metrics would otherwise count as a real approval).
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        !events.iter().any(|ev| ev.kind == "gate_resolved"),
        "a failed resume must not write a gate_resolved row"
    );
}

#[test]
fn checkpoint_approve_reacquire_failure_writes_no_ledger() {
    let store = std::sync::Arc::new(FailingReacquireStore {
        inner: InMemoryRunStore::new(),
        fail_claim: std::sync::atomic::AtomicBool::new(false),
    });
    let mut engine =
        engine_with_sops(vec![capability_checkpoint_sop("cp-claim")]).with_store(store.clone());
    let first = engine.start_run("cp-claim", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let parked = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("drive to checkpoint");
    assert!(matches!(parked, SopRunAction::CheckpointWait { .. }));
    store
        .fail_claim
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let res = engine.resolve_via_broker(
        &run_id,
        ApprovalDecision::Approve,
        ApprovalPrincipal::cli(None),
    );
    assert!(
        res.is_err(),
        "checkpoint approve must abort when the exec claim cannot be re-acquired"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the checkpoint must stay parked and re-resolvable"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        !events.iter().any(|ev| ev.kind == "gate_resolved"),
        "a failed checkpoint approve must not write a gate_resolved row: {events:?}"
    );
}

#[test]
fn checkpoint_amend_reacquire_failure_writes_no_ledger() {
    let store = std::sync::Arc::new(FailingReacquireStore {
        inner: InMemoryRunStore::new(),
        fail_claim: std::sync::atomic::AtomicBool::new(false),
    });
    let mut engine =
        engine_with_sops(vec![editable_checkpoint_sop("cp-amend-claim")]).with_store(store.clone());
    let first = engine
        .start_run(
            "cp-amend-claim",
            payload_event(r#"{"body":"model draft","repo":"o/r"}"#),
        )
        .unwrap();
    let run_id = extract_run_id(&first).to_string();
    let parked = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("drive to checkpoint");
    assert!(matches!(parked, SopRunAction::CheckpointWait { .. }));
    store
        .fail_claim
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let res = engine.resolve_via_broker(
        &run_id,
        ApprovalDecision::Amend {
            text: "operator edit".into(),
        },
        ApprovalPrincipal::cli(None),
    );
    assert!(
        res.is_err(),
        "checkpoint amend must abort when the exec claim cannot be re-acquired"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the checkpoint must stay parked and re-resolvable"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        !events.iter().any(|ev| ev.kind == "gate_resolved"),
        "a failed checkpoint amend must not write a gate_resolved row: {events:?}"
    );
}

/// Delegates to an in-memory store but can be flipped to fail every
/// `append_event`, to prove the audit-append failure path rolls back the
/// reacquired exec claim.
struct FailingAppendStore {
    inner: InMemoryRunStore,
    fail: std::sync::atomic::AtomicBool,
    fail_save: std::sync::atomic::AtomicBool,
    fail_finish: std::sync::atomic::AtomicBool,
}
impl SopRunStore for FailingAppendStore {
    fn save_run(&self, r: &PersistedRun) -> Result<(), StoreError> {
        if self.fail_save.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected save_run failure".into()));
        }
        self.inner.save_run(r)
    }
    fn save_run_with_event(&self, r: &PersistedRun, e: &SopEventRecord) -> Result<u64, StoreError> {
        if self.fail_save.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected save_run failure".into()));
        }
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected append failure".into()));
        }
        self.inner.save_run_with_event(r, e)
    }
    fn finish_run(&self, id: &str, t: &PersistedRun) -> Result<(), StoreError> {
        if self.fail_finish.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected finish failure".into()));
        }
        self.inner.finish_run(id, t)
    }
    fn finish_run_with_event(
        &self,
        id: &str,
        t: &PersistedRun,
        e: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        if self.fail_finish.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected finish failure".into()));
        }
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected append failure".into()));
        }
        self.inner.finish_run_with_event(id, t, e)
    }
    fn load_terminal_runs(
        &self,
        _limit: usize,
    ) -> Result<Vec<crate::sop::store::PersistedRun>, crate::sop::store::StoreError> {
        Ok(Vec::new())
    }
    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        self.inner.load_active_runs()
    }
    fn load_run(&self, id: &str) -> Result<Option<PersistedRun>, StoreError> {
        self.inner.load_run(id)
    }
    fn last_terminal_completed_at(&self, s: &str) -> Result<Option<String>, StoreError> {
        self.inner.last_terminal_completed_at(s)
    }
    fn try_claim_run(
        &self,
        id: &str,
        s: &str,
        p: usize,
        g: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        self.inner.try_claim_run(id, s, p, g)
    }
    fn renew_claim_for_restore(&self, id: &str, s: &str) -> Result<ClaimToken, StoreError> {
        self.inner.renew_claim_for_restore(id, s)
    }
    fn claim_counts(&self, s: &str) -> Result<(usize, usize), StoreError> {
        self.inner.claim_counts(s)
    }
    fn heartbeat_claim(&self, t: &ClaimToken) -> Result<(), StoreError> {
        self.inner.heartbeat_claim(t)
    }
    fn release_claim(&self, t: &ClaimToken) -> Result<(), StoreError> {
        self.inner.release_claim(t)
    }
    fn expired_claims(&self, n: &str) -> Result<Vec<ClaimToken>, StoreError> {
        self.inner.expired_claims(n)
    }
    fn append_event(&self, e: &SopEventRecord) -> Result<u64, StoreError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected append failure".into()));
        }
        self.inner.append_event(e)
    }
    fn list_events(&self, id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        self.inner.list_events(id)
    }
    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError> {
        self.inner.save_proposal(p)
    }
    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        self.inner.load_proposal(id)
    }
    fn list_proposals(&self, s: Option<ProposalStatus>) -> Result<Vec<ProposalRecord>, StoreError> {
        self.inner.list_proposals(s)
    }
    fn prune(&self, p: &RetentionPolicy) -> Result<usize, StoreError> {
        self.inner.prune(p)
    }
    fn health_check(&self) -> bool {
        self.inner.health_check()
    }
    fn backend(&self) -> &'static str {
        "failing-append-test"
    }
}

#[test]
fn audit_append_failure_rolls_back_reacquired_claim() {
    // A gate approval reacquires the exec claim BEFORE the audit append. If that
    // append then fails, the run stays WaitingApproval - so the reacquired claim
    // MUST be rolled back, else the parked run keeps occupying an exec slot and
    // wrongly defers later triggers.
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let a = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&a).to_string();
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "a parked run holds no exec claim"
    );
    // Now make the audit append fail, then approve.
    store.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let res = engine.resolve_gate(
        &run_id,
        ApprovalDecision::Approve,
        ApprovalPrincipal::cli(None),
    );
    assert!(
        res.is_err(),
        "resolution aborts when the audit row cannot be written"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "the reacquired claim is rolled back on audit-append failure"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the gate stays waiting (re-resolvable)"
    );
}

#[test]
fn approval_active_persist_failure_rolls_back_transition_and_ledger() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "the gate must be durably parked before this test flips save_run failures on"
    );

    store
        .fail_save
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = engine
        .resolve_gate(
            &run_id,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(Some("ZeroClawOperator".into())),
        )
        .expect_err("active transition persistence failure must reject approval");
    assert!(err.to_string().contains("injected save_run failure"));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "failed active persistence must roll the in-memory gate back to waiting"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (0, 0),
        "the claim reacquired for the rejected approval must be released"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        !events.iter().any(|ev| ev.kind == "gate_resolved"),
        "a failed active transition must not append a gate_resolved row: {events:?}"
    );
}

#[test]
fn approval_schema_reject_failure_rolls_back_without_partial_terminal_state() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let sop = test_sop(
        "schema-gate",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    );
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let event = SopEvent {
        source: SopTriggerSource::Manual,
        topic: None,
        payload: Some("{}".into()),
        timestamp: now_iso8601(),
    };
    let action = engine.start_run("schema-gate", event).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval
    );
    let mut tightened = test_sop(
        "schema-gate",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    );
    tightened.steps[0].schema = Some(StepSchema {
        input: Some(required_object_schema("ok")),
        output: None,
    });
    engine.set_sops_for_test(vec![tightened]);

    store
        .fail_finish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = engine
        .resolve_gate(
            &run_id,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(Some("ZeroClawOperator".into())),
        )
        .expect_err("terminal schema-reject commit failure must reject approval");
    assert!(err.to_string().contains("injected finish failure"));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "failed terminal persistence must restore the in-memory gate"
    );
    assert!(
        engine.finished_runs(None).is_empty(),
        "failed approval must not push a terminal run into the cache"
    );
    assert_eq!(
        store.load_run(&run_id).unwrap().unwrap().run.status,
        SopRunStatus::WaitingApproval,
        "durable state must remain the parked gate"
    );
    assert_eq!(
        store.claim_counts("schema-gate").unwrap(),
        (0, 0),
        "the reacquired claim must be released after the rejected approval"
    );
    let events = store.list_events(&run_id).unwrap();
    assert!(
        !events.iter().any(|ev| ev.kind == "gate_resolved"),
        "the rejected approval must not append gate_resolved: {events:?}"
    );
    assert!(
        !events.iter().any(|ev| ev.kind == "step_schema_reject"),
        "secondary schema events must wait for the terminal gate commit: {events:?}"
    );
}

#[test]
fn approval_route_pending_failure_rolls_back_without_step_skipped_event() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let sop = test_sop(
        "route-gate",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    );
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine.start_run("route-gate", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let mut changed = test_sop(
        "route-gate",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    );
    changed.steps[0].routing.depends_on = vec![42];
    engine.set_sops_for_test(vec![changed]);
    store
        .fail_save
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let err = engine
        .resolve_gate(
            &run_id,
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(Some("ZeroClawOperator".into())),
        )
        .expect_err("route-ineligible active commit failure must reject approval");
    assert!(err.to_string().contains("injected save_run failure"));
    let run = engine.get_run(&run_id).unwrap();
    assert_eq!(
        run.status,
        SopRunStatus::WaitingApproval,
        "failed pending persistence must restore the in-memory gate"
    );
    assert!(
        run.step_results.is_empty(),
        "pending skipped step must roll back with the gate"
    );
    assert_eq!(
        store.load_run(&run_id).unwrap().unwrap().run.status,
        SopRunStatus::WaitingApproval,
        "durable state must remain the parked gate"
    );
    assert_eq!(
        store.claim_counts("route-gate").unwrap(),
        (0, 0),
        "the reacquired claim must be released after the rejected approval"
    );
    assert!(
        engine.finished_runs(None).is_empty(),
        "route-ineligible active failure must not create terminal cache entries"
    );
    let events = store.list_events(&run_id).unwrap();
    assert!(
        !events.iter().any(|ev| ev.kind == "gate_resolved"),
        "the rejected approval must not append gate_resolved: {events:?}"
    );
    assert!(
        !events.iter().any(|ev| ev.kind == "step_skipped"),
        "secondary pending events must wait for the active gate commit: {events:?}"
    );
}

/// Delegates to an in-memory store but fails every `save_run`, to prove a park
/// does NOT release its exec claim when the parked snapshot cannot be durably
/// persisted.
struct FailingSaveStore {
    inner: InMemoryRunStore,
}
impl SopRunStore for FailingSaveStore {
    fn save_run(&self, _r: &PersistedRun) -> Result<(), StoreError> {
        Err(StoreError::Backend("injected save_run failure".into()))
    }
    fn save_run_with_event(
        &self,
        _r: &PersistedRun,
        _e: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        Err(StoreError::Backend("injected save_run failure".into()))
    }
    fn finish_run(&self, id: &str, t: &PersistedRun) -> Result<(), StoreError> {
        self.inner.finish_run(id, t)
    }
    fn finish_run_with_event(
        &self,
        id: &str,
        t: &PersistedRun,
        e: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        self.inner.finish_run_with_event(id, t, e)
    }
    fn load_terminal_runs(
        &self,
        _limit: usize,
    ) -> Result<Vec<crate::sop::store::PersistedRun>, crate::sop::store::StoreError> {
        Ok(Vec::new())
    }
    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        self.inner.load_active_runs()
    }
    fn load_run(&self, id: &str) -> Result<Option<PersistedRun>, StoreError> {
        self.inner.load_run(id)
    }
    fn last_terminal_completed_at(&self, s: &str) -> Result<Option<String>, StoreError> {
        self.inner.last_terminal_completed_at(s)
    }
    fn try_claim_run(
        &self,
        id: &str,
        s: &str,
        p: usize,
        g: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        self.inner.try_claim_run(id, s, p, g)
    }
    fn renew_claim_for_restore(&self, id: &str, s: &str) -> Result<ClaimToken, StoreError> {
        self.inner.renew_claim_for_restore(id, s)
    }
    fn claim_counts(&self, s: &str) -> Result<(usize, usize), StoreError> {
        self.inner.claim_counts(s)
    }
    fn heartbeat_claim(&self, t: &ClaimToken) -> Result<(), StoreError> {
        self.inner.heartbeat_claim(t)
    }
    fn release_claim(&self, t: &ClaimToken) -> Result<(), StoreError> {
        self.inner.release_claim(t)
    }
    fn expired_claims(&self, n: &str) -> Result<Vec<ClaimToken>, StoreError> {
        self.inner.expired_claims(n)
    }
    fn append_event(&self, e: &SopEventRecord) -> Result<u64, StoreError> {
        self.inner.append_event(e)
    }
    fn list_events(&self, id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        self.inner.list_events(id)
    }
    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError> {
        self.inner.save_proposal(p)
    }
    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        self.inner.load_proposal(id)
    }
    fn list_proposals(&self, s: Option<ProposalStatus>) -> Result<Vec<ProposalRecord>, StoreError> {
        self.inner.list_proposals(s)
    }
    fn prune(&self, p: &RetentionPolicy) -> Result<usize, StoreError> {
        self.inner.prune(p)
    }
    fn health_check(&self) -> bool {
        self.inner.health_check()
    }
    fn backend(&self) -> &'static str {
        "failing-save-test"
    }
}

#[test]
fn parked_approval_keeps_its_claim_when_the_snapshot_persist_fails() {
    // Regression: parking frees the exec slot ONLY after the parked snapshot is
    // durably persisted. If save_run fails, the claim is KEPT (fail closed) so
    // the parked run is never both claimless AND un-persisted - a crash would
    // otherwise lose the approval while newer triggers had already admitted
    // into the "freed" slot.
    let store = std::sync::Arc::new(FailingSaveStore {
        inner: InMemoryRunStore::new(),
    });
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let a = engine.start_run("s1", manual_event()).unwrap();
    assert!(
        matches!(
            a,
            SopRunAction::Pending {
                step: 1,
                ref reason,
                ..
            } if reason.contains("park snapshot not yet durably persisted")
        ),
        "a supervised first step reports durable pending while keeping its claim, got {a:?}"
    );
    let run_id = extract_run_id(&a).to_string();
    assert!(
        engine.is_park_persist_pending(&run_id),
        "the failed park persist must be tracked until a later retry succeeds"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the canonical run must stay parked while the transient action reports Pending"
    );
    let advance = engine.advance_step(
        &run_id,
        SopStepResult {
            step_number: 1,
            status: SopStepStatus::Completed,
            output: "should not advance".into(),
            started_at: now_iso8601(),
            completed_at: Some(now_iso8601()),
            effective_agent: None,
            tool_calls: Vec::new(),
        },
    );
    assert!(
        advance.is_err(),
        "sop_advance must not bypass an approval gate whose park snapshot is still pending"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the exec claim is KEPT when the parked snapshot cannot be persisted"
    );
    assert!(
        !engine.can_start("s1"),
        "the held slot must not admit a new trigger while the park is un-persisted"
    );
}

#[test]
fn checkpoint_park_keeps_its_claim_when_the_snapshot_persist_fails() {
    // Same fail-closed guarantee as the approval-park case, for the
    // deterministic-checkpoint park site.
    let store = std::sync::Arc::new(FailingSaveStore {
        inner: InMemoryRunStore::new(),
    });
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]).with_store(store.clone());
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(
        matches!(
            action,
            SopRunAction::Pending {
                step: 2,
                ref reason,
                ..
            } if reason.contains("park snapshot not yet durably persisted")
        ),
        "a checkpoint park reports durable pending while keeping its claim, got {action:?}"
    );
    assert!(
        engine.is_park_persist_pending(&run_id),
        "the failed checkpoint persist must be tracked until a later retry succeeds"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the canonical run must stay parked while the transient action reports Pending"
    );
    let advance = engine.advance_step(
        &run_id,
        SopStepResult {
            step_number: 2,
            status: SopStepStatus::Completed,
            output: "should not advance".into(),
            started_at: now_iso8601(),
            completed_at: Some(now_iso8601()),
            effective_agent: None,
            tool_calls: Vec::new(),
        },
    );
    assert!(
        advance.is_err(),
        "sop_advance must not bypass a checkpoint whose park snapshot is still pending"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (1, 1),
        "the exec claim is KEPT when the checkpoint snapshot cannot be persisted"
    );
    assert!(
        !engine.can_start("det-cp"),
        "the held slot must not admit a new trigger while the checkpoint is un-persisted"
    );
}

#[test]
fn resolve_gate_refuses_to_approve_while_park_persist_is_pending() {
    // A failed park persist keeps the exec claim and downgrades the exposed
    // action to Pending, because there is no durably parked approval row to
    // resolve yet. Any manual approval attempt must fail without releasing
    // the pre-existing kept claim.
    let store = std::sync::Arc::new(FailingSaveStore {
        inner: InMemoryRunStore::new(),
    });
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let a = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&a).to_string();
    assert!(
        engine.is_park_persist_pending(&run_id),
        "the failed park persist must be tracked while the claim is kept"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the exec claim is KEPT when the parked snapshot cannot be persisted"
    );

    let res = engine.resolve_gate(
        &run_id,
        ApprovalDecision::Approve,
        ApprovalPrincipal::cli(Some("ZeroClawOperator".into())),
    );
    assert!(
        res.is_err(),
        "approval must not resume while the park's snapshot is not yet durably persisted"
    );
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the pre-existing kept claim must survive the refused approval attempt"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::WaitingApproval,
        "the run stays parked, re-resolvable once the park persists"
    );
}

#[test]
fn approve_step_refuses_to_resume_while_checkpoint_persist_is_pending() {
    // Same class of regression as the approval park case, for the
    // deterministic-checkpoint resume path.
    let store = std::sync::Arc::new(FailingSaveStore {
        inner: InMemoryRunStore::new(),
    });
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]).with_store(store.clone());
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(
        matches!(
            action,
            SopRunAction::Pending {
                step: 2,
                ref reason,
                ..
            } if reason.contains("park snapshot not yet durably persisted")
        ),
        "the failed checkpoint persist must surface as durable pending, got {action:?}"
    );
    assert!(
        engine.is_park_persist_pending(&run_id),
        "the failed checkpoint persist must be tracked while the claim is kept"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (1, 1),
        "the exec claim is KEPT when the checkpoint snapshot cannot be persisted"
    );

    let res = engine.approve_step(&run_id);
    assert!(
        res.is_err(),
        "resume must be refused while the checkpoint's snapshot is not yet durably persisted"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (1, 1),
        "the pre-existing kept claim must survive the refused resume attempt"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the run stays parked, re-resolvable once the checkpoint persists"
    );
}

#[test]
fn resume_deterministic_run_refuses_to_resume_while_checkpoint_persist_is_pending() {
    // Same class of regression, via the restore-path entry point.
    let store = std::sync::Arc::new(FailingSaveStore {
        inner: InMemoryRunStore::new(),
    });
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]).with_store(store.clone());
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(
        matches!(
            action,
            SopRunAction::Pending {
                step: 2,
                ref reason,
                ..
            } if reason.contains("park snapshot not yet durably persisted")
        ),
        "the failed checkpoint persist must surface as durable pending, got {action:?}"
    );
    assert!(
        engine.is_park_persist_pending(&run_id),
        "the failed checkpoint persist must be tracked while the claim is kept"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (1, 1),
        "the exec claim is KEPT when the checkpoint snapshot cannot be persisted"
    );

    let mut step_outputs = HashMap::new();
    step_outputs.insert(1u32, serde_json::json!("s1-out"));
    let state = DeterministicRunState {
        run_id: run_id.clone(),
        sop_name: "det-cp".to_string(),
        last_completed_step: 1,
        total_steps: 3,
        step_outputs,
        persisted_at: now_iso8601(),
        llm_calls_saved: 0,
        paused_at_checkpoint: true,
    };

    let res = engine.resume_deterministic_run(state);
    assert!(
        res.is_err(),
        "resume must be refused while the checkpoint's snapshot is not yet durably persisted"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (1, 1),
        "the pre-existing kept claim must survive the refused resume attempt"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the run stays parked, re-resolvable once the checkpoint persists"
    );
}

/// A test store with REAL, test-controllable claim-lease semantics - unlike
/// `InMemoryRunStore`, whose claims carry a permanently empty (never-expiring)
/// lease. Can inject either `save_run` or terminal `finish_run` failures while
/// keeping real expiring claims, so maintenance tests can prove retained
/// claims are renewed rather than reaped.
struct FailingSaveLeasedStore {
    inner: InMemoryRunStore,
    claims: std::sync::Mutex<std::collections::HashMap<String, ClaimToken>>,
    fail_save: std::sync::atomic::AtomicBool,
    fail_next_save: std::sync::atomic::AtomicBool,
    fail_finish: std::sync::atomic::AtomicBool,
    fail_marker: std::sync::atomic::AtomicBool,
    fail_release: std::sync::atomic::AtomicBool,
    fail_has_retained: std::sync::atomic::AtomicBool,
}
impl FailingSaveLeasedStore {
    fn healthy() -> Self {
        Self {
            inner: InMemoryRunStore::new(),
            claims: std::sync::Mutex::new(std::collections::HashMap::new()),
            fail_save: std::sync::atomic::AtomicBool::new(false),
            fail_next_save: std::sync::atomic::AtomicBool::new(false),
            fail_finish: std::sync::atomic::AtomicBool::new(false),
            fail_marker: std::sync::atomic::AtomicBool::new(false),
            fail_release: std::sync::atomic::AtomicBool::new(false),
            fail_has_retained: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn new() -> Self {
        Self {
            inner: InMemoryRunStore::new(),
            claims: std::sync::Mutex::new(std::collections::HashMap::new()),
            fail_save: std::sync::atomic::AtomicBool::new(true),
            fail_next_save: std::sync::atomic::AtomicBool::new(false),
            fail_finish: std::sync::atomic::AtomicBool::new(false),
            fail_marker: std::sync::atomic::AtomicBool::new(false),
            fail_release: std::sync::atomic::AtomicBool::new(false),
            fail_has_retained: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn finish_fails() -> Self {
        Self {
            inner: InMemoryRunStore::new(),
            claims: std::sync::Mutex::new(std::collections::HashMap::new()),
            fail_save: std::sync::atomic::AtomicBool::new(false),
            fail_next_save: std::sync::atomic::AtomicBool::new(false),
            fail_finish: std::sync::atomic::AtomicBool::new(true),
            fail_marker: std::sync::atomic::AtomicBool::new(false),
            fail_release: std::sync::atomic::AtomicBool::new(false),
            fail_has_retained: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn finish_and_marker_fail() -> Self {
        Self {
            inner: InMemoryRunStore::new(),
            claims: std::sync::Mutex::new(std::collections::HashMap::new()),
            fail_save: std::sync::atomic::AtomicBool::new(false),
            fail_next_save: std::sync::atomic::AtomicBool::new(false),
            fail_finish: std::sync::atomic::AtomicBool::new(true),
            fail_marker: std::sync::atomic::AtomicBool::new(true),
            fail_release: std::sync::atomic::AtomicBool::new(false),
            fail_has_retained: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn fail_next_save(&self) {
        self.fail_next_save
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    /// Inject a claim-release failure: the next (and subsequent) `release_claim`
    /// calls error AND leave the claim row in place, modelling a transient store
    /// fault during the checkpoint-denial continuation release.
    fn set_fail_release(&self, on: bool) {
        self.fail_release
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }
    /// Inject a retention-marker inspection failure: `has_retained_terminal_rollback_claim`
    /// errors, modelling a transient claim-store read fault during restore.
    fn set_fail_has_retained(&self, on: bool) {
        self.fail_has_retained
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }
    fn should_fail_save(&self) -> bool {
        self.fail_save.load(std::sync::atomic::Ordering::SeqCst)
            || self
                .fail_next_save
                .swap(false, std::sync::atomic::Ordering::SeqCst)
    }
    /// Force an existing claim's lease into the past, simulating a claim that
    /// was taken but never subsequently renewed.
    fn expire_claim_now(&self, run_id: &str) {
        if let Some(token) = self.claims.lock().unwrap().get_mut(run_id) {
            token.lease_expires = "2000-01-01T00:00:00Z".to_string();
        }
    }
}
impl SopRunStore for FailingSaveLeasedStore {
    fn save_run(&self, r: &PersistedRun) -> Result<(), StoreError> {
        if self.should_fail_save() {
            Err(StoreError::Backend("injected save_run failure".into()))
        } else {
            self.inner.save_run(r)
        }
    }
    fn save_run_with_event(&self, r: &PersistedRun, e: &SopEventRecord) -> Result<u64, StoreError> {
        if self.should_fail_save() {
            Err(StoreError::Backend("injected save_run failure".into()))
        } else {
            self.inner.save_run_with_event(r, e)
        }
    }
    fn finish_run(&self, id: &str, t: &PersistedRun) -> Result<(), StoreError> {
        if self.fail_finish.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected finish failure".into()));
        }
        self.inner.finish_run(id, t)?;
        self.claims.lock().unwrap().remove(id);
        Ok(())
    }
    fn finish_run_with_event(
        &self,
        id: &str,
        t: &PersistedRun,
        e: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        if self.fail_finish.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected finish failure".into()));
        }
        let seq = self.inner.finish_run_with_event(id, t, e)?;
        self.claims.lock().unwrap().remove(id);
        Ok(seq)
    }
    fn load_terminal_runs(
        &self,
        _limit: usize,
    ) -> Result<Vec<crate::sop::store::PersistedRun>, crate::sop::store::StoreError> {
        Ok(Vec::new())
    }
    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        self.inner.load_active_runs()
    }
    fn load_run(&self, id: &str) -> Result<Option<PersistedRun>, StoreError> {
        self.inner.load_run(id)
    }
    fn last_terminal_completed_at(&self, s: &str) -> Result<Option<String>, StoreError> {
        self.inner.last_terminal_completed_at(s)
    }
    fn try_claim_run(
        &self,
        run_id: &str,
        sop_name: &str,
        per_sop_cap: usize,
        global_cap: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        let mut claims = self.claims.lock().unwrap();
        if claims.contains_key(run_id) {
            return Ok(None);
        }
        let active_for_sop = claims.values().filter(|c| c.sop_name == sop_name).count();
        if active_for_sop >= per_sop_cap || claims.len() >= global_cap {
            return Ok(None);
        }
        let now = now_iso8601();
        let token = ClaimToken {
            run_id: run_id.to_string(),
            sop_name: sop_name.to_string(),
            claimed_at: now,
            // Far-future: this test drives expiry explicitly via
            // `expire_claim_now`/`heartbeat_claim`, not real elapsed time.
            lease_expires: "2099-01-01T00:00:00Z".to_string(),
            holder: "leased-test".to_string(),
        };
        claims.insert(run_id.to_string(), token.clone());
        Ok(Some(token))
    }
    fn renew_claim_for_restore(
        &self,
        run_id: &str,
        sop_name: &str,
    ) -> Result<ClaimToken, StoreError> {
        let token = ClaimToken {
            run_id: run_id.to_string(),
            sop_name: sop_name.to_string(),
            claimed_at: now_iso8601(),
            lease_expires: "2099-01-01T00:00:00Z".to_string(),
            holder: "leased-test".to_string(),
        };
        self.claims
            .lock()
            .unwrap()
            .insert(run_id.to_string(), token.clone());
        Ok(token)
    }
    fn mark_claim_retained_after_terminal_rollback(&self, run_id: &str) -> Result<(), StoreError> {
        if self.fail_marker.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("injected marker failure".into()));
        }
        if let Some(token) = self.claims.lock().unwrap().get_mut(run_id) {
            token.holder = crate::sop::store::RETAINED_TERMINAL_ROLLBACK_HOLDER.to_string();
        }
        Ok(())
    }
    fn has_retained_terminal_rollback_claim(&self, run_id: &str) -> Result<bool, StoreError> {
        if self
            .fail_has_retained
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Backend(
                "injected retention-marker inspection failure".into(),
            ));
        }
        Ok(self
            .claims
            .lock()
            .unwrap()
            .get(run_id)
            .is_some_and(|token| {
                token.holder == crate::sop::store::RETAINED_TERMINAL_ROLLBACK_HOLDER
            }))
    }
    fn claim_counts(&self, sop_name: &str) -> Result<(usize, usize), StoreError> {
        let claims = self.claims.lock().unwrap();
        let per_sop = claims.values().filter(|c| c.sop_name == sop_name).count();
        Ok((per_sop, claims.len()))
    }
    fn heartbeat_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        if let Some(existing) = self.claims.lock().unwrap().get_mut(&token.run_id) {
            existing.lease_expires = "2099-01-01T00:00:00Z".to_string();
        }
        Ok(())
    }
    fn release_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        if self.fail_release.load(std::sync::atomic::Ordering::SeqCst) {
            // Model a transient store fault: the claim row survives the failed
            // release so a swallowed failure would leak it.
            return Err(StoreError::Backend("injected release failure".into()));
        }
        self.claims.lock().unwrap().remove(&token.run_id);
        Ok(())
    }
    fn expired_claims(&self, now_iso: &str) -> Result<Vec<ClaimToken>, StoreError> {
        let claims = self.claims.lock().unwrap();
        Ok(claims
            .values()
            .filter(|c| c.lease_expires.as_str() <= now_iso)
            .cloned()
            .collect())
    }
    fn append_event(&self, e: &SopEventRecord) -> Result<u64, StoreError> {
        self.inner.append_event(e)
    }
    fn list_events(&self, id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        self.inner.list_events(id)
    }
    fn save_proposal(&self, p: &ProposalRecord) -> Result<(), StoreError> {
        self.inner.save_proposal(p)
    }
    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        self.inner.load_proposal(id)
    }
    fn list_proposals(&self, s: Option<ProposalStatus>) -> Result<Vec<ProposalRecord>, StoreError> {
        self.inner.list_proposals(s)
    }
    fn prune(&self, p: &RetentionPolicy) -> Result<usize, StoreError> {
        self.inner.prune(p)
    }
    fn health_check(&self) -> bool {
        self.inner.health_check()
    }
    fn backend(&self) -> &'static str {
        "failing-save-leased-test"
    }
}

#[test]
fn parked_claim_kept_after_failed_persist_survives_maintenance_reap() {
    // Keeping the claim on a failed park
    // persist is only fail-closed if the kept claim's lease keeps being
    // renewed. Without tracking it in `claims_pending_persist`,
    // `heartbeat_active_claims` skips it (parked status), its lease goes
    // un-renewed, and `reap_expired_claims` reclaims it once the lease is in
    // the past - silently undoing the fail-closed keep and letting a newer
    // trigger over-admit.
    let store = std::sync::Arc::new(FailingSaveLeasedStore::new());
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let a = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&a).to_string();
    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the exec claim is KEPT when the parked snapshot cannot be persisted"
    );

    // Simulate real time passing with no heartbeat since the original claim:
    // force the lease into the past, as if an hour had gone by unrenewed.
    store.expire_claim_now(&run_id);

    // A maintenance tick must renew the kept claim's lease (via
    // `retry_pending_park_persists` + `heartbeat_active_claims`) before the
    // reaper runs, so the now-expired-in-the-past lease gets refreshed rather
    // than reclaimed.
    engine.run_maintenance_tick();

    assert_eq!(
        store.claim_counts("s1").unwrap(),
        (1, 1),
        "the kept claim must survive the maintenance tick's reaper - it must be \
         heartbeated, not silently reclaimed once its (unrenewed) lease is in the past"
    );
    assert!(
        !engine.can_start("s1"),
        "the slot must still be held after the tick - the park is still un-persisted"
    );
}

#[test]
fn checkpoint_state_file_failure_keeps_run_executing_and_claim_renewed() {
    let store = std::sync::Arc::new(FailingSaveLeasedStore::healthy());
    let mut sop = deterministic_sop("det-cp-state-file-fails");
    let location_file = std::env::temp_dir().join(format!(
        "zc-state-location-file-{}",
        now_iso8601().replace(':', "-")
    ));
    std::fs::write(&location_file, "not a directory").unwrap();
    sop.location = Some(location_file.clone());

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-state-file-fails", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();

    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .expect_err("checkpoint state-file write must fail for a file-valued location");
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Running,
        "state-file failure must not park the run before the checkpoint is durable"
    );
    assert!(
        !engine.is_park_persist_pending(&run_id),
        "state-file failure happens before park-persist retry tracking is needed"
    );
    assert_eq!(
        store.claim_counts("det-cp-state-file-fails").unwrap(),
        (1, 1),
        "the still-running run keeps its execution claim"
    );

    store.expire_claim_now(&run_id);
    let summary = engine.run_maintenance_tick();
    assert_eq!(
        summary.reaped_claims, 0,
        "maintenance must heartbeat the still-running claim before reaping"
    );
    assert_eq!(
        store.claim_counts("det-cp-state-file-fails").unwrap(),
        (1, 1),
        "the execution claim remains live after maintenance"
    );

    let _ = std::fs::remove_file(location_file);
}

#[test]
fn denied_checkpoint_terminal_rollback_claim_survives_restart_and_maintenance_reap() {
    let store = std::sync::Arc::new(FailingSaveLeasedStore::finish_fails());
    let mut sop = deterministic_sop("det-cp-deny-finish-lease");
    sop.max_concurrent = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-finish-lease", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();

    let checkpoint = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(checkpoint, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        store.claim_counts("det-cp-deny-finish-lease").unwrap(),
        (0, 0),
        "a durably parked checkpoint starts without an execution claim"
    );

    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("terminal persistence failure must reject the denial");
    assert!(err.to_string().contains("injected finish failure"));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-finish-lease").unwrap(),
        (1, 1),
        "the failed terminal write keeps the reacquired claim fail-closed"
    );

    let mut restored_sop = deterministic_sop("det-cp-deny-finish-lease");
    restored_sop.max_concurrent = 1;
    let mut restored = engine_with_sops(vec![restored_sop]).with_store(store.clone());
    restored.restore_runs();
    assert_eq!(
        restored.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "restart must restore the parked checkpoint run"
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-finish-lease").unwrap(),
        (1, 1),
        "restore must preserve the retained terminal-rollback claim"
    );
    assert!(
        !restored.can_start("det-cp-deny-finish-lease"),
        "the retained claim must still block duplicate admission after restart"
    );

    store.expire_claim_now(&run_id);
    let summary = restored.run_maintenance_tick();

    assert_eq!(
        summary.reaped_claims, 0,
        "maintenance must heartbeat the retained terminal-rollback claim before reaping"
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-finish-lease").unwrap(),
        (1, 1),
        "the retained checkpoint-denial claim must survive an expired-lease sweep"
    );
    assert!(
        !restored.can_start("det-cp-deny-finish-lease"),
        "the retained claim must keep the execution slot held until the denial is retried"
    );
}

#[test]
fn denied_checkpoint_marker_failure_aborts_without_retained_claim() {
    let store = std::sync::Arc::new(FailingSaveLeasedStore::finish_and_marker_fail());
    let mut sop = deterministic_sop("det-cp-deny-marker-fail");
    sop.max_concurrent = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-marker-fail", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();

    let checkpoint = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(checkpoint, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        store.claim_counts("det-cp-deny-marker-fail").unwrap(),
        (0, 0),
        "a durably parked checkpoint starts without an execution claim"
    );

    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("marker persistence failure must reject the denial before terminal write");
    assert!(err.to_string().contains("injected marker failure"));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "marker failure leaves the checkpoint parked and re-resolvable"
    );
    assert!(
        !store.has_retained_terminal_rollback_claim(&run_id).unwrap(),
        "the injected marker failure must leave no durable marker"
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-marker-fail").unwrap(),
        (0, 0),
        "marker failure releases the reacquired claim instead of retaining it without a marker"
    );

    let mut restored_sop = deterministic_sop("det-cp-deny-marker-fail");
    restored_sop.max_concurrent = 1;
    let mut restored = engine_with_sops(vec![restored_sop]).with_store(store.clone());
    restored.restore_runs();
    assert_eq!(
        restored.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "restart must restore the parked checkpoint run normally"
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-marker-fail").unwrap(),
        (0, 0),
        "restore must not invent retention for an unmarked parked checkpoint"
    );
    assert!(
        restored.can_start("det-cp-deny-marker-fail"),
        "an unmarked parked checkpoint must not consume the execution slot after restart"
    );
}

#[test]
fn denied_checkpoint_goto_checkpoint_releases_claim_after_recovered_park_persist() {
    let store = std::sync::Arc::new(FailingSaveLeasedStore::healthy());
    let mut sop = deterministic_sop("det-cp-deny-goto-cp");
    sop.steps[1].on_failure = StepFailure::Goto { step: 4 };
    sop.steps.push(SopStep {
        number: 4,
        title: "Second checkpoint".into(),
        body: "Pause again".into(),
        suggested_tools: vec![],
        requires_confirmation: false,
        kind: SopStepKind::Checkpoint,
        schema: None,
        ..SopStep::default()
    });
    sop.max_concurrent = 1;

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-goto-cp", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    let checkpoint = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(checkpoint, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        store.claim_counts("det-cp-deny-goto-cp").unwrap(),
        (0, 0),
        "a durably parked checkpoint starts without an execution claim"
    );

    store.fail_next_save();
    let action = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect("denial should route to the second checkpoint");
    assert!(
        matches!(
            action,
            SopRunAction::Pending {
                step: 4,
                ref reason,
                ..
            } if reason.contains("park snapshot not yet durably persisted")
        ),
        "the first park save failure is still surfaced to the caller, got {action:?}"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the routed denial ends parked at the second checkpoint"
    );
    assert!(
        !engine.is_park_persist_pending(&run_id),
        "the outer denial persist completed the parked snapshot and must clear retry tracking"
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-goto-cp").unwrap(),
        (0, 0),
        "the outer denial persist must release the exec claim for the parked route target"
    );
    assert!(
        engine.can_start("det-cp-deny-goto-cp"),
        "the parked route target must not consume the SOP concurrency slot"
    );
}

#[test]
fn deny_checkpoint_goto_continuation_release_failure_aborts_without_pinning_slot() {
    // A denied checkpoint whose failure route (Goto) lands on ANOTHER
    // checkpoint CONTINUES the run — it did not terminal-rollback. If clearing the
    // stale terminal-rollback retention marker (the parked-continuation claim
    // release) fails, the denial must NOT return Ok with a live durable marker on a
    // continued run: it fails closed (rolls back + surfaces the error) and drops the
    // in-memory retention so the lease reaper frees the slot instead of the engine
    // renewing it forever.
    let store = std::sync::Arc::new(FailingSaveLeasedStore::healthy());
    let mut sop = deterministic_sop("det-cp-deny-goto-release-fail");
    sop.steps[1].on_failure = StepFailure::Goto { step: 4 };
    sop.steps.push(SopStep {
        number: 4,
        title: "Second checkpoint".into(),
        body: "Pause again".into(),
        suggested_tools: vec![],
        requires_confirmation: false,
        kind: SopStepKind::Checkpoint,
        schema: None,
        ..SopStep::default()
    });
    sop.max_concurrent = 1;

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-goto-release-fail", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    let checkpoint = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(checkpoint, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        store.claim_counts("det-cp-deny-goto-release-fail").unwrap(),
        (0, 0),
        "a durably parked checkpoint starts without an execution claim"
    );

    store.set_fail_release(true);
    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("a failed continuation claim release must reject the denial");
    assert!(
        err.to_string()
            .contains("failed to release exec claim after routing checkpoint denial"),
        "unexpected error: {err}"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the rejected continuation rolls back to the pre-decision checkpoint"
    );
    assert!(
        !engine
            .claims_retained_after_terminal_rollback
            .contains(&run_id),
        "a CONTINUED run must not be tracked as a terminal-rollback retention (else it is heartbeated forever)"
    );

    // The stale claim lingers durably only until the reaper collects it: it is NOT
    // heartbeated (not retained, run is parked), so once its lease lapses a
    // maintenance tick frees the slot — no permanent double-pin.
    store.set_fail_release(false);
    store.expire_claim_now(&run_id);
    let _ = engine.run_maintenance_tick();
    assert_eq!(
        store.claim_counts("det-cp-deny-goto-release-fail").unwrap(),
        (0, 0),
        "the stale continuation claim is reaped, not renewed forever"
    );
    assert!(
        engine.can_start("det-cp-deny-goto-release-fail"),
        "the freed slot is available again after the stale claim is reaped"
    );
}

#[test]
fn restore_reconciles_stale_terminal_rollback_marker_on_retried_checkpoint() {
    // Crash-window reconcile: a denied checkpoint whose failure route (Retry)
    // re-parks at the SAME checkpoint CONTINUES the run. If the continuation claim
    // release fails and the daemon then restarts before the lease reaper runs, the
    // durable terminal-rollback marker survives on a run that already recorded a
    // Failed result for its current step. `restore_runs` must recognise that marker
    // as stale (a completed continuation, not a genuine terminal rollback) and
    // RELEASE it rather than renew it forever.
    let store = std::sync::Arc::new(FailingSaveLeasedStore::healthy());
    let mut sop = deterministic_sop("det-cp-deny-retry-reconcile");
    sop.steps[1].on_failure = StepFailure::Retry { max: 2 };
    sop.max_concurrent = 1;

    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-retry-reconcile", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    let checkpoint = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(checkpoint, SopRunAction::CheckpointWait { .. }));

    store.set_fail_release(true);
    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("a failed continuation claim release must reject the denial");
    assert!(
        err.to_string()
            .contains("failed to release exec claim after routing checkpoint denial"),
        "unexpected error: {err}"
    );
    // Precondition for the crash-window: the release failure left the durable marker
    // live on the (Retry-)continued run.
    assert!(
        store.has_retained_terminal_rollback_claim(&run_id).unwrap(),
        "the failed release leaves a stale durable terminal-rollback marker"
    );

    // Simulate a restart: the transient release fault has cleared.
    store.set_fail_release(false);
    let mut restored = engine_with_sops(vec![{
        let mut s = deterministic_sop("det-cp-deny-retry-reconcile");
        s.steps[1].on_failure = StepFailure::Retry { max: 2 };
        s.max_concurrent = 1;
        s
    }])
    .with_store(store.clone());
    restored.restore_runs();

    assert_eq!(
        restored.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "restart restores the parked checkpoint run normally"
    );
    assert!(
        !store.has_retained_terminal_rollback_claim(&run_id).unwrap(),
        "restore must reconcile the stale marker away, not renew it"
    );
    assert!(
        !restored
            .claims_retained_after_terminal_rollback
            .contains(&run_id),
        "a reconciled run must not be tracked for terminal-rollback heartbeating"
    );
    assert_eq!(
        store.claim_counts("det-cp-deny-retry-reconcile").unwrap(),
        (0, 0),
        "the stale terminal-rollback claim is released on restore"
    );
    assert!(
        restored.can_start("det-cp-deny-retry-reconcile"),
        "a continued parked checkpoint must not keep the execution slot after restart"
    );
}

#[test]
fn resolve_gate_clears_routed_non_contiguous_step() {
    // End-to-end: a routed SOP waiting at step 5 (steps numbered 1 and 5) must
    // clear by step NUMBER. Before the fix, clear_waiting_gate read step index 4
    // of a 2-element vec -> None -> Err, but only AFTER resolve_gate reacquired
    // the claim and wrote gate_resolved.
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.steps = vec![
        SopStep {
            number: 1,
            title: "a".into(),
            ..SopStep::default()
        },
        SopStep {
            number: 5,
            title: "b".into(),
            ..SopStep::default()
        },
    ];
    let mut engine =
        engine_with_sops(vec![sop]).with_store(std::sync::Arc::new(InMemoryRunStore::new()));
    let now = now_iso8601();
    engine.active_runs.insert(
        "r1".to_string(),
        SopRun {
            run_id: "r1".to_string(),
            sop_name: "s1".to_string(),
            trigger_event: manual_event(),
            frame_marker_id: "m".to_string(),
            status: SopRunStatus::WaitingApproval,
            current_step: 5,
            total_steps: 2,
            started_at: now.clone(),
            completed_at: None,
            step_results: Vec::new(),
            waiting_since: Some(now),
            llm_calls_saved: 0,
            revision: 0,
            revision_base: 0,
        },
    );
    let out = engine
        .resolve_gate(
            "r1",
            ApprovalDecision::Approve,
            ApprovalPrincipal::cli(None),
        )
        .expect("routed gate clears without error");
    match out {
        crate::sop::approval::ResolveOutcome::Resumed(a) => match *a {
            SopRunAction::ExecuteStep { step, .. } => assert_eq!(
                step.number, 5,
                "resumes the step whose NUMBER is 5, not vec index 5"
            ),
            other => panic!("expected ExecuteStep, got {other:?}"),
        },
        other => panic!("expected Resumed, got {other:?}"),
    }
}

#[test]
fn persist_runs_defaults_on() {
    // A1 durability leg: parked HITL runs must survive a restart out of the box.
    assert!(
        SopConfig::default().persist_runs,
        "persist_runs must default on so a pending approval is not lost on restart"
    );
}

// ── A2: admission policy (SopAdmissionPolicy) ─────────────────

/// A single-slot SOP that stays executing (Auto, multi-step) after start, so
/// its exec slot is occupied for admission-policy assertions.
fn exec_filled_engine(policy: SopAdmissionPolicy) -> (SopEngine, String) {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.max_concurrent = 1;
    sop.admission_policy = policy;
    let mut engine = engine_with_sops(vec![sop]).with_store(store);
    let a = engine.start_run("s1", manual_event()).unwrap();
    assert!(
        matches!(a, SopRunAction::ExecuteStep { .. }),
        "auto start executes (holds its exec slot)"
    );
    let run_id = extract_run_id(&a).to_string();
    (engine, run_id)
}

#[test]
fn admission_policy_defaults_to_parallel() {
    let sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    assert_eq!(sop.admission_policy, SopAdmissionPolicy::Parallel);
    assert_eq!(sop.max_pending_approvals, 0);
}

#[test]
fn parallel_admits_when_a_slot_is_free() {
    let engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    assert_eq!(engine.evaluate_admission("s1"), SopAdmission::Admit);
}

#[test]
fn parallel_defers_when_exec_slots_full() {
    // Never drops on concurrency: a second trigger is deferred for backpressure.
    let (engine, _) = exec_filled_engine(SopAdmissionPolicy::Parallel);
    assert!(matches!(
        engine.evaluate_admission("s1"),
        SopAdmission::Defer { .. }
    ));
}

#[test]
fn drop_policy_drops_when_exec_slots_full() {
    // Explicit opt-in to the legacy fire-and-forget behavior.
    let (engine, _) = exec_filled_engine(SopAdmissionPolicy::Drop);
    assert!(matches!(
        engine.evaluate_admission("s1"),
        SopAdmission::Drop { .. }
    ));
}

#[test]
fn hold_defers_while_a_run_is_in_flight() {
    let (engine, _) = exec_filled_engine(SopAdmissionPolicy::Hold);
    assert!(matches!(
        engine.evaluate_admission("s1"),
        SopAdmission::Defer { .. }
    ));
}

#[test]
fn coalesce_folds_into_the_in_flight_run() {
    let (engine, run1) = exec_filled_engine(SopAdmissionPolicy::Coalesce);
    match engine.evaluate_admission("s1") {
        SopAdmission::Coalesce { existing_run_id } => assert_eq!(existing_run_id, run1),
        other => panic!("expected Coalesce, got {other:?}"),
    }
}

#[test]
fn pending_pool_bound_defers_new_triggers() {
    // Exec slots are free, but the pending-approval pool is full (a Supervised run
    // parks immediately) -> a new trigger defers (backpressure), never dropped.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 5;
    sop.max_pending_approvals = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store);
    let a = engine.start_run("s1", manual_event()).unwrap();
    assert!(matches!(a, SopRunAction::WaitApproval { .. }));
    assert!(matches!(
        engine.evaluate_admission("s1"),
        SopAdmission::Defer { .. }
    ));
}

#[test]
fn pending_pool_bound_preempts_coalesce_into_a_parked_run() {
    // The `max_pending_approvals` cap check in `evaluate_admission` runs BEFORE
    // the per-policy match, so it must defer a fresh trigger even under
    // Coalesce - even though `first_active_run_for_sop` WOULD find the parked
    // run to fold onto - rather than let Coalesce bypass the pending-approval
    // backpressure bound. Exec slots stay free (max_concurrent=5); only the
    // pending pool (max_pending_approvals=1) is at capacity.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = test_sop("s1", SopExecutionMode::Supervised, SopPriority::Normal);
    sop.max_concurrent = 5;
    sop.max_pending_approvals = 1;
    sop.admission_policy = SopAdmissionPolicy::Coalesce;
    let mut engine = engine_with_sops(vec![sop]).with_store(store);
    let a = engine.start_run("s1", manual_event()).unwrap();
    assert!(matches!(a, SopRunAction::WaitApproval { .. }));
    let run_id = extract_run_id(&a).to_string();

    // Sanity: absent the cap, Coalesce would find this same parked run to fold
    // onto - so the Defer below is the cap preempting Coalesce, not a case
    // where there was nothing to coalesce with.
    assert_eq!(engine.first_active_run_for_sop("s1"), Some(run_id));

    assert!(
        matches!(engine.evaluate_admission("s1"), SopAdmission::Defer { .. }),
        "the pending-approval cap must defer, not Coalesce past it"
    );
}

// ── Eviction ──────────────────────────────────────

#[test]
fn max_finished_runs_evicts_oldest() {
    let mut engine = SopEngine::new(SopConfig {
        max_finished_runs: 2,
        ..SopConfig::default()
    });
    // SOP with 1 step so each run completes in one advance
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps = vec![sop.steps[0].clone()];
    sop.max_concurrent = 10;
    engine.sops = vec![sop];

    // Complete 3 runs
    let mut finished_ids = Vec::new();
    for _ in 0..3 {
        let action = engine.start_run("s1", manual_event()).unwrap();
        let rid = extract_run_id(&action).to_string();
        engine
            .advance_step(
                &rid,
                SopStepResult {
                    step_number: 1,
                    status: SopStepStatus::Completed,
                    output: "ok".into(),
                    started_at: now_iso8601(),
                    completed_at: Some(now_iso8601()),
                    effective_agent: None,
                    tool_calls: Vec::new(),
                },
            )
            .unwrap();
        finished_ids.push(rid);
    }

    // Only 2 should be kept (max_finished_runs=2)
    let finished = engine.finished_runs(None);
    assert_eq!(
        finished.len(),
        2,
        "eviction should cap at max_finished_runs"
    );
    // Oldest (first) run should be evicted, newest two remain
    assert_eq!(finished[0].run_id, finished_ids[1]);
    assert_eq!(finished[1].run_id, finished_ids[2]);
}

#[test]
fn max_finished_runs_zero_means_unlimited() {
    let mut engine = SopEngine::new(SopConfig {
        max_finished_runs: 0,
        ..SopConfig::default()
    });
    let mut sop = test_sop("s1", SopExecutionMode::Auto, SopPriority::Normal);
    sop.steps = vec![sop.steps[0].clone()];
    sop.max_concurrent = 10;
    engine.sops = vec![sop];

    for _ in 0..5 {
        let action = engine.start_run("s1", manual_event()).unwrap();
        let rid = extract_run_id(&action).to_string();
        engine
            .advance_step(
                &rid,
                SopStepResult {
                    step_number: 1,
                    status: SopStepStatus::Completed,
                    output: "ok".into(),
                    started_at: now_iso8601(),
                    completed_at: Some(now_iso8601()),
                    effective_agent: None,
                    tool_calls: Vec::new(),
                },
            )
            .unwrap();
    }

    assert_eq!(engine.finished_runs(None).len(), 5, "zero means unlimited");
}

#[test]
fn waiting_since_cleared_on_approve() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Supervised,
        SopPriority::Normal,
    )]);
    let action = engine.start_run("s1", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    approve_gate_cli(&mut engine, &run_id);

    let run = engine.get_run(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::Running);
    assert!(run.waiting_since.is_none());
}

// ── Deterministic execution ─────────────────────────

fn deterministic_sop(name: &str) -> Sop {
    Sop {
        name: name.into(),
        description: format!("Deterministic SOP: {name}"),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Step one".into(),
                body: "Do step one".into(),
                suggested_tools: vec![],
                requires_confirmation: false,
                kind: SopStepKind::Execute,
                schema: None,
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Checkpoint".into(),
                body: "Pause for approval".into(),
                suggested_tools: vec![],
                requires_confirmation: false,
                kind: SopStepKind::Checkpoint,
                schema: None,
                ..SopStep::default()
            },
            SopStep {
                number: 3,
                title: "Step three".into(),
                body: "Final step".into(),
                suggested_tools: vec![],
                requires_confirmation: false,
                kind: SopStepKind::Execute,
                schema: None,
                ..SopStep::default()
            },
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
        agent: None,
    }
}

#[test]
fn deterministic_start_returns_deterministic_step() {
    let mut engine = engine_with_sops(vec![deterministic_sop("det-sop")]);
    let action = engine.start_run("det-sop", manual_event()).unwrap();
    assert!(
        matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 1),
        "First action should be DeterministicStep for step 1"
    );
    let run_id = extract_run_id(&action).to_string();
    assert!(run_id.starts_with("det-"));
}

#[test]
fn deterministic_start_routes_through_start_run() {
    let mut engine = engine_with_sops(vec![deterministic_sop("det-sop")]);
    // start_run should auto-route to start_deterministic_run
    let action = engine.start_run("det-sop", manual_event()).unwrap();
    assert!(matches!(action, SopRunAction::DeterministicStep { .. }));
}

#[test]
fn deterministic_advance_pipes_output() {
    let mut engine = engine_with_sops(vec![deterministic_sop("det-sop")]);
    let action = engine.start_run("det-sop", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Advance step 1 with output
    let output = serde_json::json!({"result": "step1_done"});
    let action = engine
        .advance_deterministic_step(&run_id, output.clone(), None)
        .unwrap();

    // Step 2 is a checkpoint — should pause
    assert!(
        matches!(action, SopRunAction::CheckpointWait { ref step, .. } if step.number == 2),
        "Step 2 (checkpoint) should return CheckpointWait"
    );
}

#[test]
fn deterministic_checkpoint_pauses_run() {
    let mut engine = engine_with_sops(vec![deterministic_sop("det-sop")]);
    let action = engine.start_run("det-sop", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Complete step 1
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!({"ok": true}), None)
        .unwrap();

    // Should be at checkpoint
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));

    // Run should be PausedCheckpoint
    let run = engine.get_run(&run_id).unwrap();
    assert_eq!(run.status, SopRunStatus::PausedCheckpoint);
    assert!(run.waiting_since.is_some());
}

#[test]
fn durable_policied_checkpoint_delivers_exactly_one_request_notice() {
    use zeroclaw_config::schema::ApprovalPolicyConfig;

    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    let mut config = SopConfig::default();
    config.approval.policies.insert(
        "prod".to_string(),
        ApprovalPolicyConfig {
            required_group: None,
            quorum: 0,
            request_route: Some("discord.ops:checkpoint".to_string()),
            escalation_route: None,
        },
    );
    let mut sop = deterministic_sop("det-routed-checkpoint");
    sop.steps[1].policy = Some("prod".to_string());
    let mut engine = engine_with_config_sops(config, vec![sop]).with_approval_broker(
        std::sync::Arc::new(crate::sop::approval::ApprovalBroker::with_route(adapter)),
    );

    let first = engine
        .start_run("det-routed-checkpoint", manual_event())
        .unwrap();
    let run_id = extract_run_id(&first).to_string();
    assert!(calls.lock().unwrap().is_empty());

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("step-one"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));

    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "the durable checkpoint sends once");
    assert_eq!(
        recorded[0],
        (
            crate::sop::approval::ApprovalNoticeKind::Request,
            "discord.ops:checkpoint".to_string(),
            run_id,
            "det-routed-checkpoint".to_string(),
            2,
        )
    );
}

#[test]
fn policied_checkpoint_retry_delivers_once_after_persist_succeeds() {
    use zeroclaw_config::schema::ApprovalPolicyConfig;

    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let mut config = SopConfig::default();
    config.approval.policies.insert(
        "prod".to_string(),
        ApprovalPolicyConfig {
            required_group: None,
            quorum: 0,
            request_route: Some("discord.ops:checkpoint".to_string()),
            escalation_route: None,
        },
    );
    let mut sop = deterministic_sop("det-routed-checkpoint-retry");
    sop.steps[1].policy = Some("prod".to_string());
    let mut engine = engine_with_config_sops(config, vec![sop])
        .with_store(store.clone())
        .with_approval_broker(std::sync::Arc::new(
            crate::sop::approval::ApprovalBroker::with_route(adapter),
        ));

    let first = engine
        .start_run("det-routed-checkpoint-retry", manual_event())
        .unwrap();
    let run_id = extract_run_id(&first).to_string();
    store
        .fail_save
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("step-one"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::Pending { .. }));
    assert!(
        calls.lock().unwrap().is_empty(),
        "an undurable checkpoint must not send"
    );
    assert!(engine.is_park_persist_pending(&run_id));

    store
        .fail_save
        .store(false, std::sync::atomic::Ordering::SeqCst);
    engine.run_maintenance_tick();
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "the successful retry sends exactly once"
    );
    engine.run_maintenance_tick();
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "later maintenance ticks must not resend"
    );
}

#[test]
fn deterministic_completion_tracks_savings() {
    let mut sop = deterministic_sop("det-sop");
    // Simplify: 2 execute steps, no checkpoint
    sop.steps = vec![
        SopStep {
            number: 1,
            title: "Step one".into(),
            body: "Do it".into(),
            suggested_tools: vec![],
            requires_confirmation: false,
            kind: SopStepKind::Execute,
            schema: None,
            ..SopStep::default()
        },
        SopStep {
            number: 2,
            title: "Step two".into(),
            body: "Do it too".into(),
            suggested_tools: vec![],
            requires_confirmation: false,
            kind: SopStepKind::Execute,
            schema: None,
            ..SopStep::default()
        },
    ];
    let mut engine = engine_with_sops(vec![sop]);

    let action = engine.start_run("det-sop", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Complete step 1
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::DeterministicStep { .. }));

    // Complete step 2
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s2"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::Completed { .. }));

    // Check savings
    let savings = engine.deterministic_savings();
    assert_eq!(savings.total_runs, 1);
    assert_eq!(savings.total_llm_calls_saved, 2);
}

#[test]
fn deterministic_non_deterministic_sop_rejected() {
    let mut engine = engine_with_sops(vec![test_sop(
        "s1",
        SopExecutionMode::Auto,
        SopPriority::Normal,
    )]);
    let result = engine.start_deterministic_run("s1", manual_event());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not in deterministic mode")
    );
}

#[test]
fn new_engine_without_sops_dir_stays_empty() {
    let config = SopConfig {
        sops_dir: None,
        ..Default::default()
    };
    let engine = SopEngine::new(config);
    assert!(
        engine.sops().is_empty(),
        "engine without sops_dir must have no SOPs"
    );
}

#[test]
fn reload_loads_sops_when_sops_dir_is_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let sops_dir = tmp.path().join("my_sops");
    let sop_subdir = sops_dir.join("test-sop");
    std::fs::create_dir_all(&sop_subdir).unwrap();

    std::fs::write(
        sop_subdir.join("SOP.toml"),
        r#"
[sop]
name = "test-sop"
description = "A test SOP"
version = "1.0.0"

[[triggers]]
type = "manual"
"#,
    )
    .unwrap();

    let config = SopConfig {
        sops_dir: Some(sops_dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut engine = SopEngine::new(config);
    engine.reload(tmp.path());
    assert_eq!(
        engine.sops().len(),
        1,
        "reload must populate SOPs from disk"
    );
    assert_eq!(engine.sops()[0].name, "test-sop");
}

fn deterministic_sop_all_execute(name: &str) -> Sop {
    Sop {
        name: name.into(),
        description: format!("Deterministic SOP: {name}"),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Step one".into(),
                body: "Do step one".into(),
                suggested_tools: vec![],
                requires_confirmation: false,
                kind: SopStepKind::Execute,
                schema: None,
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Step two".into(),
                body: "Do step two".into(),
                suggested_tools: vec![],
                requires_confirmation: false,
                kind: SopStepKind::Execute,
                schema: None,
                ..SopStep::default()
            },
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        admission_policy: crate::sop::types::SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
        agent: None,
    }
}

#[test]
fn deterministic_run_drives_to_completion_through_advance_step() {
    let mut engine = engine_with_sops(vec![deterministic_sop_all_execute("det-run")]);
    let action = engine.start_run("det-run", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 1));

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "step1-output".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 2),
        "advance_step on a deterministic run must route to the deterministic path"
    );

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 2,
                status: SopStepStatus::Completed,
                output: "step2-output".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        matches!(action, SopRunAction::Completed { .. }),
        "deterministic run should complete after its final step"
    );
}

#[test]
fn deterministic_run_uses_explicit_next_routing() {
    let mut sop = deterministic_sop_all_execute("det-route");
    sop.steps.push(SopStep {
        number: 3,
        title: "Step three".into(),
        body: "Do step three".into(),
        kind: SopStepKind::Execute,
        ..SopStep::default()
    });
    sop.steps[0].routing.next = Some(3);
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("det-route", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    assert!(matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 1));

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!({"ok": true}), None)
        .unwrap();

    assert!(
        matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 3),
        "deterministic routing should select explicit step 3"
    );
}

#[test]
fn deterministic_routed_checkpoint_persists_actual_last_completed_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sop = deterministic_sop_all_execute("det-route-cp");
    sop.location = Some(tmp.path().to_path_buf());
    sop.steps.push(SopStep {
        number: 3,
        title: "Checkpoint three".into(),
        body: "Pause at step three".into(),
        kind: SopStepKind::Checkpoint,
        ..SopStep::default()
    });
    sop.steps[0].routing.next = Some(3);
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("det-route-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!({"step": 1}), None)
        .unwrap();
    let (state_file, step_number) = match action {
        SopRunAction::CheckpointWait {
            state_file, step, ..
        } => (state_file, step.number),
        other => {
            assert!(
                matches!(other, SopRunAction::CheckpointWait { .. }),
                "expected routed checkpoint wait"
            );
            return;
        }
    };
    assert_eq!(step_number, 3);

    let state = SopEngine::load_deterministic_state(&state_file).unwrap();

    assert_eq!(state.last_completed_step, 1);
    assert!(state.step_outputs.contains_key(&1));
    assert!(!state.step_outputs.contains_key(&2));
}

#[test]
fn deterministic_failed_step_fails_run_through_advance_step() {
    let mut engine = engine_with_sops(vec![deterministic_sop_all_execute("det-fail")]);
    let action = engine.start_run("det-fail", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Failed,
                output: "boom".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        matches!(action, SopRunAction::Failed { .. }),
        "a failed deterministic step must fail the run"
    );
}

#[test]
fn deterministic_output_schema_failure_fails_run() {
    let mut sop = deterministic_sop_all_execute("det-schema");
    sop.steps[0].schema = Some(StepSchema {
        input: None,
        output: Some(required_object_schema("ok")),
    });
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine.start_run("det-schema", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!({}), None)
        .unwrap();

    assert!(
        matches!(action, SopRunAction::Failed { ref reason, .. } if reason.contains("output schema validation failed"))
    );
    assert!(engine.active_runs().is_empty());
    assert_eq!(engine.finished_runs(None)[0].status, SopRunStatus::Failed);
}

#[test]
fn deterministic_advance_step_preserves_caller_timestamps() {
    let mut engine = engine_with_sops(vec![deterministic_sop_all_execute("det-ts")]);
    let action = engine.start_run("det-ts", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let started = "2026-01-01T00:00:00Z".to_string();
    let completed = "2026-01-01T00:00:42Z".to_string();
    engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 1,
                status: SopStepStatus::Completed,
                output: "step1-output".into(),
                started_at: started.clone(),
                completed_at: Some(completed.clone()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();

    let recorded = engine
        .get_run(&run_id)
        .unwrap()
        .step_results
        .iter()
        .find(|r| r.step_number == 1)
        .expect("step 1 result recorded");
    assert_eq!(recorded.started_at, started);
    assert_eq!(recorded.completed_at, Some(completed));
}

#[test]
fn deterministic_checkpoint_resumes_through_approve_step() {
    // approve_step owns the deterministic PausedCheckpoint resume (the
    // sop_approve tool routes here when resolve_gate reports NotWaiting). A run
    // paused at a checkpoint must resume through it, not bail. deterministic_sop
    // is step1=Execute, step2=Checkpoint, step3=Execute.
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]);
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    // Advance step 1 -> pauses at the step-2 checkpoint.
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    // Approve the checkpoint via the public path -> yields step 3.
    let action = engine.approve_step(&run_id).unwrap();
    assert!(
        matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 3),
        "approving a deterministic checkpoint must resume to the next step"
    );

    // Advance step 3 -> run completes.
    let action = engine
        .advance_step(
            &run_id,
            SopStepResult {
                step_number: 3,
                status: SopStepStatus::Completed,
                output: "s3-out".into(),
                started_at: now_iso8601(),
                completed_at: Some(now_iso8601()),
                effective_agent: None,
                tool_calls: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        matches!(action, SopRunAction::Completed { .. }),
        "deterministic run should complete after the post-checkpoint step"
    );
}

#[test]
fn approve_step_fails_closed_when_sop_removed_while_parked() {
    // Regression: `approve_step` used to reacquire the exec claim and flip the
    // run to `Running` BEFORE `advance_deterministic_step` resolved the SOP and
    // its current step - so an operator removing the SOP definition while a
    // deterministic run sat parked at a checkpoint would strand the run in
    // `Running`, holding a claim, unable to ever advance (the resolve still
    // errors, but the mutation had already committed). The
    // `can_advance_deterministic_step` pre-flight must make this fail closed
    // with the run left untouched at `PausedCheckpoint` instead.
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]);
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    // Operator removes the SOP definition out from under the parked run.
    engine.set_sops_for_test(vec![]);

    let res = engine.approve_step(&run_id);
    assert!(
        res.is_err(),
        "approve_step must fail closed when the SOP is gone, not strand the run"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "a failed-closed approve must leave the run resumable, not stuck in Running"
    );

    // The exec slot was not leaked: restore the SOP and a fresh trigger must
    // admit. With max_concurrent=1, a claim leaked by the parked run would
    // defer this instead.
    engine.set_sops_for_test(vec![deterministic_sop("det-cp")]);
    let fresh = engine.start_run("det-cp", manual_event()).unwrap();
    assert!(
        matches!(fresh, SopRunAction::DeterministicStep { .. }),
        "a fresh run must admit - no phantom exec slot held by the parked run: {fresh:?}"
    );
}

#[test]
fn resume_deterministic_run_fails_closed_when_sop_shrunk_while_parked() {
    // Regression: `resume_deterministic_run` resolved the waiting step
    // (`resolve_sop_step`) AFTER it had already reacquired the claim and
    // flipped the run to `Running` - so an operator shrinking the SOP
    // (removing the step the persisted checkpoint state points at) while the
    // run sat parked would strand it in `Running`, holding a claim, with no
    // way to make progress. The pre-flight must fail closed BEFORE the claim
    // and the mutation.
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]);
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    // Operator shrinks the SOP: step 1 (the persisted last-completed step) no
    // longer exists, though the SOP itself is still loaded under the same name.
    let mut shrunk = deterministic_sop("det-cp");
    shrunk.steps.clear();
    engine.set_sops_for_test(vec![shrunk]);

    let mut step_outputs = HashMap::new();
    step_outputs.insert(1u32, serde_json::json!("s1-out"));
    let state = DeterministicRunState {
        run_id: run_id.clone(),
        sop_name: "det-cp".to_string(),
        last_completed_step: 1,
        total_steps: 3,
        step_outputs,
        persisted_at: now_iso8601(),
        llm_calls_saved: 0,
        paused_at_checkpoint: true,
    };

    let res = engine.resume_deterministic_run(state);
    assert!(
        res.is_err(),
        "resume must fail closed when the waiting step no longer exists"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "a failed-closed resume must leave the run resumable, not stuck in Running"
    );

    // The exec slot was not leaked: restore the SOP and a fresh trigger must
    // admit. With max_concurrent=1, a claim leaked by the parked run would
    // defer this instead.
    engine.set_sops_for_test(vec![deterministic_sop("det-cp")]);
    let fresh = engine.start_run("det-cp", manual_event()).unwrap();
    assert!(
        matches!(fresh, SopRunAction::DeterministicStep { .. }),
        "a fresh run must admit - no phantom exec slot held by the parked run: {fresh:?}"
    );
}

/// `capability(noop) -> checkpoint -> capability(noop)`: the shape the
/// checkpoint bridge exists for (an approved write-back tail, e.g.
/// `forge.comment`, executing headlessly after an out-of-band approval).
fn capability_checkpoint_sop(name: &str) -> Sop {
    let cap_step = |number: u32| SopStep {
        number,
        title: format!("Capability {number}"),
        kind: SopStepKind::Capability,
        capability: Some("noop".into()),
        ..SopStep::default()
    };
    Sop {
        name: name.into(),
        description: "cap -> checkpoint -> cap".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            cap_step(1),
            SopStep {
                number: 2,
                title: "Checkpoint".into(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            },
            cap_step(3),
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
        agent: None,
    }
}

struct CountingForgeCommentAdapter {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl super::super::capability::ForgeCommentAdapter for CountingForgeCommentAdapter {
    fn post_comment(
        &self,
        _channel: Option<&str>,
        _repo: &str,
        _number: u64,
        _body: &str,
    ) -> std::result::Result<(), String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

struct MutatesForgePayload;

impl super::super::capability::SopCapability for MutatesForgePayload {
    fn id(&self) -> &'static str {
        "mutate.forge"
    }

    fn describe(&self) -> super::super::capability::CapabilityInfo {
        super::super::capability::CapabilityInfo {
            id: self.id(),
            description: "Change the approved forge comment body",
            deterministic: true,
            idempotent: true,
            reversible: false,
            supports_retry: false,
            required_permissions: Vec::new(),
            input_schema: None,
            output_schema: None,
        }
    }

    fn execute(
        &self,
        _ctx: super::super::capability::CapabilityContext,
        _input: serde_json::Value,
    ) -> anyhow::Result<super::super::capability::CapabilityResult> {
        Ok(super::super::capability::CapabilityResult::success(
            serde_json::json!({
                "repo": "o/r",
                "number": 7,
                "body": "mutated after approval",
                "looped": true,
            }),
        ))
    }
}

fn forge_comment_registry(
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<SopCapabilityRegistry> {
    let mut registry = super::super::capability::SopCapabilityRegistry::with_builtins();
    let adapter: Arc<dyn super::super::capability::ForgeCommentAdapter> =
        Arc::new(CountingForgeCommentAdapter { calls });
    registry.register(super::super::capability::ForgeCommentCapability::new(Some(
        adapter,
    )));
    Arc::new(registry)
}

fn forge_comment_registry_with_mutator(
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<SopCapabilityRegistry> {
    let mut registry = super::super::capability::SopCapabilityRegistry::with_builtins();
    registry.register(MutatesForgePayload);
    let adapter: Arc<dyn super::super::capability::ForgeCommentAdapter> =
        Arc::new(CountingForgeCommentAdapter { calls });
    registry.register(super::super::capability::ForgeCommentCapability::new(Some(
        adapter,
    )));
    Arc::new(registry)
}

fn forge_comment_event() -> SopEvent {
    SopEvent {
        source: SopTriggerSource::Manual,
        topic: None,
        payload: Some(
            serde_json::json!({
                "channel": "git.main",
                "repo": "o/r",
                "number": 7,
                "body": "triage approved",
            })
            .to_string(),
        ),
        timestamp: now_iso8601(),
    }
}

fn forge_comment_step(number: u32) -> SopStep {
    forge_comment_step_with_channel(number, "git.main")
}

fn forge_comment_step_with_channel(number: u32, channel: &str) -> SopStep {
    SopStep {
        number,
        title: format!("Forge comment {number}"),
        kind: SopStepKind::Capability,
        capability: Some("forge.comment".into()),
        capability_input: Some(serde_json::json!({
            "channel": channel,
            "repo": "o/r",
            "number": 7,
            "body": "triage approved",
        })),
        ..SopStep::default()
    }
}

fn direct_forge_comment_sop(name: &str) -> Sop {
    Sop {
        name: name.into(),
        description: "forge without checkpoint".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![forge_comment_step(1)],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        agent: None,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
    }
}

fn checkpoint_forge_comment_sop(name: &str) -> Sop {
    checkpoint_forge_comment_sop_with_channel(name, "git.main")
}

fn checkpoint_forge_comment_sop_with_channel(name: &str, channel: &str) -> Sop {
    Sop {
        name: name.into(),
        description: "checkpoint -> forge".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Checkpoint".into(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            },
            forge_comment_step_with_channel(2, channel),
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        agent: None,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
    }
}

fn two_checkpoint_forge_comment_sop(name: &str) -> Sop {
    Sop {
        name: name.into(),
        description: "checkpoint -> noop -> checkpoint -> forge".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "First checkpoint".into(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Bridge".into(),
                kind: SopStepKind::Capability,
                capability: Some("noop".into()),
                ..SopStep::default()
            },
            SopStep {
                number: 3,
                title: "Second checkpoint".into(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            },
            forge_comment_step(4),
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        agent: None,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
    }
}

fn checkpoint_mutates_before_forge_comment_sop(name: &str) -> Sop {
    Sop {
        name: name.into(),
        description: "checkpoint -> mutator -> forge".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Checkpoint".into(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Mutate approved body".into(),
                kind: SopStepKind::Capability,
                capability: Some("mutate.forge".into()),
                ..SopStep::default()
            },
            forge_comment_step(3),
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        agent: None,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
    }
}

fn same_step_revisit_forge_comment_sop(name: &str) -> Sop {
    Sop {
        name: name.into(),
        description: "checkpoint -> marker -> checkpoint -> forge".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Checkpoint".into(),
                kind: SopStepKind::Checkpoint,
                routing: crate::sop::step_contract::StepRouting {
                    switch: vec![
                        crate::sop::step_contract::SwitchRule {
                            name: "second-visit".into(),
                            when: Some("$.steps.2.looped == true".into()),
                            goto: Some(3),
                        },
                        crate::sop::step_contract::SwitchRule {
                            name: "first-visit".into(),
                            when: None,
                            goto: Some(2),
                        },
                    ],
                    ..Default::default()
                },
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Mark loop".into(),
                kind: SopStepKind::Capability,
                capability: Some("mutate.forge".into()),
                routing: crate::sop::step_contract::StepRouting {
                    next: Some(1),
                    ..Default::default()
                },
                ..SopStep::default()
            },
            forge_comment_step(3),
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        agent: None,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
    }
}

struct FailFirstFinishStore {
    inner: InMemoryRunStore,
    fail_next_finish: std::sync::atomic::AtomicBool,
}

impl FailFirstFinishStore {
    fn new() -> Self {
        Self {
            inner: InMemoryRunStore::new(),
            fail_next_finish: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl SopRunStore for FailFirstFinishStore {
    fn save_run(&self, run: &PersistedRun) -> Result<(), StoreError> {
        self.inner.save_run(run)
    }

    fn save_run_with_event(
        &self,
        run: &PersistedRun,
        event: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        self.inner.save_run_with_event(run, event)
    }

    fn finish_run(&self, run_id: &str, terminal: &PersistedRun) -> Result<(), StoreError> {
        if self
            .fail_next_finish
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Backend(
                "injected first terminal persistence failure".into(),
            ));
        }
        self.inner.finish_run(run_id, terminal)
    }

    fn finish_run_with_event(
        &self,
        run_id: &str,
        terminal: &PersistedRun,
        event: &SopEventRecord,
    ) -> Result<u64, StoreError> {
        self.inner.finish_run_with_event(run_id, terminal, event)
    }

    fn load_active_runs(&self) -> Result<Vec<PersistedRun>, StoreError> {
        self.inner.load_active_runs()
    }

    fn load_terminal_runs(&self, limit: usize) -> Result<Vec<PersistedRun>, StoreError> {
        self.inner.load_terminal_runs(limit)
    }

    fn load_run(&self, run_id: &str) -> Result<Option<PersistedRun>, StoreError> {
        self.inner.load_run(run_id)
    }

    fn last_terminal_completed_at(&self, sop_name: &str) -> Result<Option<String>, StoreError> {
        self.inner.last_terminal_completed_at(sop_name)
    }

    fn try_claim_run(
        &self,
        run_id: &str,
        sop_name: &str,
        per_sop_cap: usize,
        global_cap: usize,
    ) -> Result<Option<ClaimToken>, StoreError> {
        self.inner
            .try_claim_run(run_id, sop_name, per_sop_cap, global_cap)
    }

    fn renew_claim_for_restore(
        &self,
        run_id: &str,
        sop_name: &str,
    ) -> Result<ClaimToken, StoreError> {
        self.inner.renew_claim_for_restore(run_id, sop_name)
    }

    fn claim_counts(&self, sop_name: &str) -> Result<(usize, usize), StoreError> {
        self.inner.claim_counts(sop_name)
    }

    fn heartbeat_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        self.inner.heartbeat_claim(token)
    }

    fn release_claim(&self, token: &ClaimToken) -> Result<(), StoreError> {
        self.inner.release_claim(token)
    }

    fn expired_claims(&self, now_iso: &str) -> Result<Vec<ClaimToken>, StoreError> {
        self.inner.expired_claims(now_iso)
    }

    fn append_event(&self, event: &SopEventRecord) -> Result<u64, StoreError> {
        self.inner.append_event(event)
    }

    fn list_events(&self, run_id: &str) -> Result<Vec<SopEventRecord>, StoreError> {
        self.inner.list_events(run_id)
    }

    fn save_proposal(&self, proposal: &ProposalRecord) -> Result<(), StoreError> {
        self.inner.save_proposal(proposal)
    }

    fn load_proposal(&self, id: &str) -> Result<Option<ProposalRecord>, StoreError> {
        self.inner.load_proposal(id)
    }

    fn list_proposals(
        &self,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<ProposalRecord>, StoreError> {
        self.inner.list_proposals(status)
    }

    fn prune(&self, policy: &RetentionPolicy) -> Result<usize, StoreError> {
        self.inner.prune(policy)
    }

    fn health_check(&self) -> bool {
        self.inner.health_check()
    }

    fn backend(&self) -> &'static str {
        "fail-first-finish-test"
    }
}

#[test]
fn intake_gate_pipeline_pipes_the_trigger_payload_through_a_step_one_checkpoint() {
    // A step-one intake gate can use `checkpoint -> capability -> ...`. The
    // step-1 checkpoint has no prior step result, so its resume must pipe
    // the TRIGGER PAYLOAD forward (mapping identical to `step_input_value`),
    // not Null — otherwise the first work step is starved of the event.
    let sop = Sop {
        name: "intake-gate".into(),
        description: "checkpoint before work".into(),
        version: "1.0.0".into(),
        priority: SopPriority::Normal,
        execution_mode: SopExecutionMode::Deterministic,
        triggers: vec![SopTrigger::Manual],
        steps: vec![
            SopStep {
                number: 1,
                title: "Intake gate".into(),
                kind: SopStepKind::Checkpoint,
                ..SopStep::default()
            },
            SopStep {
                number: 2,
                title: "Work".into(),
                kind: SopStepKind::Capability,
                capability: Some("noop".into()),
                ..SopStep::default()
            },
        ],
        cooldown_secs: 0,
        max_concurrent: 1,
        location: None,
        deterministic: true,
        admission_policy: SopAdmissionPolicy::Parallel,
        max_pending_approvals: 0,
        agent: None,
    };
    let mut engine = engine_with_sops(vec![sop]);
    let event = SopEvent {
        source: SopTriggerSource::Channel,
        topic: Some("git.main:issues.opened".into()),
        payload: Some(r#"{"repo":"o/r","number":7}"#.into()),
        timestamp: now_iso8601(),
    };
    let first = engine.start_run("intake-gate", event).unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "run must park at the step-1 intake gate: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("intake gate approve resolves");
    assert!(
        matches!(
            outcome,
            super::super::approval::BrokerOutcome::Resolved(
                super::super::approval::ResolveOutcome::Resumed(_)
            )
        ),
        "expected Resolved(Resumed), got {outcome:?}"
    );
    // The noop capability echoes its input: the recorded step-2 output must
    // BE the trigger payload, proving it crossed the step-1 checkpoint.
    let run = engine
        .last_finished_run("intake-gate")
        .expect("run completed");
    assert_eq!(run.status, SopRunStatus::Completed);
    let step2 = run
        .step_results
        .iter()
        .find(|r| r.step_number == 2)
        .expect("step 2 recorded");
    let parsed: serde_json::Value =
        serde_json::from_str(&step2.output).expect("step-2 output is json");
    assert_eq!(
        parsed,
        serde_json::json!({"repo": "o/r", "number": 7}),
        "the trigger payload must survive the step-1 checkpoint"
    );
}

#[test]
fn forge_comment_refuses_without_prior_ledgered_checkpoint() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![direct_forge_comment_sop("forge-direct")])
        .with_capabilities(forge_comment_registry(Arc::clone(&calls)));

    let first = engine.start_run("forge-direct", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let final_action = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("direct forge run should fail closed");

    assert!(
        matches!(final_action, SopRunAction::Failed { .. }),
        "direct forge.comment must fail closed, got {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "forge adapter must not be called before a ledgered checkpoint"
    );
    let run = engine
        .last_finished_run("forge-direct")
        .expect("failed run should be retained");
    let result = run
        .step_results
        .iter()
        .find(|result| result.step_number == 1)
        .expect("forge step result recorded");
    assert_eq!(result.status, SopStepStatus::Failed);
    assert!(
        result.output.contains("immediately preceding checkpoint"),
        "failure should name the missing authorization invariant: {result:?}"
    );
}

#[test]
fn forge_comment_runs_after_checkpoint_resolution() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![checkpoint_forge_comment_sop("forge-approved")])
        .with_capabilities(forge_comment_registry(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-approved", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "forge run must park at the checkpoint before writing: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint approve resolves");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(final_action),
    ) = outcome
    else {
        panic!("expected Resolved(Resumed), got {outcome:?}");
    };

    assert!(
        matches!(*final_action, SopRunAction::Completed { .. }),
        "approved forge tail must complete headlessly: {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "forge adapter should run exactly once after checkpoint approval"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().any(|ev| ev.kind == "gate_resolved"),
        "checkpoint resolution must append the ledger row before forge.comment executes: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|ev| ev.kind == "capability_effect_completed"),
        "forge.comment success must write a durable effect marker: {events:?}"
    );
    let run = engine
        .last_finished_run("forge-approved")
        .expect("run reached the finished list");
    assert_eq!(run.status, SopRunStatus::Completed);
}

#[test]
fn forge_comment_replay_after_terminal_persist_failure_does_not_repost() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store: Arc<dyn SopRunStore> = Arc::new(FailFirstFinishStore::new());
    let mut engine = engine_with_sops(vec![checkpoint_forge_comment_sop("forge-replay")])
        .with_store(Arc::clone(&store))
        .with_capabilities(forge_comment_registry(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-replay", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "forge run must park before writing: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let first_error = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect_err("injected terminal persistence failure must propagate");
    assert!(
        first_error
            .to_string()
            .contains("terminal persistence failed"),
        "injected terminal persistence failure must fail closed: {first_error}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "first approval performs the public forge write once"
    );
    assert!(
        engine.get_run(&run_id).is_some(),
        "terminal persistence failure keeps the in-memory run active"
    );
    assert!(
        engine.last_finished_run("forge-replay").is_none(),
        "terminal persistence failure must not move the run to finished_runs"
    );
    let first_events = engine.run_events(&run_id).unwrap();
    assert_eq!(
        first_events
            .iter()
            .filter(|ev| ev.kind == "capability_effect_started")
            .count(),
        1,
        "forge write must have a durable started marker before the public send: {first_events:?}"
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|ev| ev.kind == "capability_effect_completed")
            .count(),
        1,
        "forge write must have a durable completed marker after the public send: {first_events:?}"
    );

    drop(engine);

    let mut restarted = engine_with_sops(vec![checkpoint_forge_comment_sop("forge-replay")])
        .with_store(Arc::clone(&store))
        .with_capabilities(forge_comment_registry(Arc::clone(&calls)));
    restarted.restore_runs();
    assert_eq!(
        restarted.get_run(&run_id).map(|run| run.status),
        Some(SopRunStatus::Running),
        "restart restores the durable in-flight capability state"
    );

    let restored_run = restarted.get_run(&run_id).cloned().unwrap();
    let restored_sop = restarted.get_sop("forge-replay").cloned().unwrap();
    let restored_step = restored_sop
        .steps
        .iter()
        .find(|step| step.capability_id() == Some("forge.comment"))
        .cloned()
        .unwrap();
    let restored_step_number = restored_step.number;
    let replay_input = step_input_value(&restored_run, restored_step_number);
    let capability_input = restored_step.capability_call_input(replay_input.clone());
    assert!(
        restarted.forge_comment_authorized_by_prior_checkpoint(
            &restored_sop,
            &run_id,
            restored_step_number,
            &capability_input,
        ),
        "restored checkpoint authorization must still match: run={restored_run:?}, input={capability_input:?}, events={:?}",
        restarted.run_events(&run_id).unwrap()
    );
    let replay_action = restarted
        .dispatch_deterministic_step(&run_id, &restored_sop, restored_step_number, replay_input)
        .expect("restored forge step dispatches");
    let second_final_action = restarted
        .drive_headless_deterministic(&run_id, replay_action)
        .expect("restored deterministic capability resumes");
    assert!(
        matches!(second_final_action, SopRunAction::Completed { .. }),
        "replay with a completed effect marker must complete without posting again: {second_final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "replay after terminal persistence failure must not create a second public comment"
    );
    let events = restarted.run_events(&run_id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|ev| ev.kind == "capability_effect_started")
            .count(),
        1,
        "replay must not write a second started marker: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|ev| ev.kind == "capability_effect_completed")
            .count(),
        1,
        "replay must reuse the completed effect marker: {events:?}"
    );
    let run = restarted
        .last_finished_run("forge-replay")
        .expect("replayed run reaches finished list");
    assert_eq!(run.status, SopRunStatus::Completed);
}

#[test]
fn forge_comment_rejects_agent_resolved_checkpoint() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![checkpoint_forge_comment_sop("forge-agent")])
        .with_capabilities(forge_comment_registry(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-agent", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "forge run must park at the checkpoint before writing: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::agent("triage-agent"),
        )
        .expect("agent checkpoint approve resolves through default approval mode");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(final_action),
    ) = outcome
    else {
        panic!("expected Resolved(Resumed), got {outcome:?}");
    };

    assert!(
        matches!(*final_action, SopRunAction::Failed { .. }),
        "agent-cleared checkpoint must not authorize forge.comment: {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "forge adapter must not run after an agent-sourced checkpoint approval"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().any(|event| {
            event.kind == "gate_resolved"
                && event.payload.get("source").and_then(|value| value.as_str()) == Some("agent")
        }),
        "test must prove the rejected ledger row was agent-sourced: {events:?}"
    );
    let run = engine
        .last_finished_run("forge-agent")
        .expect("failed run should be retained");
    let result = run
        .step_results
        .iter()
        .find(|result| result.step_number == 2)
        .expect("forge step result recorded");
    assert_eq!(result.status, SopStepStatus::Failed);
    assert!(
        result.output.contains("immediately preceding checkpoint"),
        "failure should name the checkpoint authorization invariant: {result:?}"
    );
}
#[test]
fn forge_comment_rejects_payload_mutated_after_checkpoint() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![checkpoint_mutates_before_forge_comment_sop(
        "forge-mutated",
    )])
    .with_capabilities(forge_comment_registry_with_mutator(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-mutated", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "run must park at the checkpoint before any forge write: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint approve resolves");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(final_action),
    ) = outcome
    else {
        panic!("expected Resolved(Resumed), got {outcome:?}");
    };

    assert!(
        matches!(*final_action, SopRunAction::Failed { .. }),
        "mutated forge payload must require a new checkpoint: {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "forge adapter must not run after an intervening capability changes the approved body"
    );
    let run = engine
        .last_finished_run("forge-mutated")
        .expect("failed run should be retained");
    let result = run
        .step_results
        .iter()
        .find(|result| result.step_number == 3)
        .expect("forge step result recorded");
    assert_eq!(result.status, SopStepStatus::Failed);
    assert!(
        result
            .output
            .contains("exact repo, number, body, and channel"),
        "failure should name the exact payload invariant: {result:?}"
    );
}

#[test]
fn forge_comment_rejects_channel_changed_after_checkpoint() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![checkpoint_forge_comment_sop_with_channel(
        "forge-channel-mismatch",
        "git.admin",
    )])
    .with_capabilities(forge_comment_registry(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-channel-mismatch", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "run must park at the checkpoint before any forge write: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint approve resolves");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(final_action),
    ) = outcome
    else {
        panic!("expected Resolved(Resumed), got {outcome:?}");
    };

    assert!(
        matches!(*final_action, SopRunAction::Failed { .. }),
        "changed forge channel must require a new checkpoint: {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "forge adapter must not run when the approved channel differs from the static forge target"
    );
    let run = engine
        .last_finished_run("forge-channel-mismatch")
        .expect("failed run should be retained");
    let result = run
        .step_results
        .iter()
        .find(|result| result.step_number == 2)
        .expect("forge step result recorded");
    assert_eq!(result.status, SopStepStatus::Failed);
    assert!(
        result
            .output
            .contains("exact repo, number, body, and channel"),
        "failure should name the exact target invariant: {result:?}"
    );
}

#[test]
fn forge_comment_rejects_stale_ledger_from_prior_checkpoint() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![two_checkpoint_forge_comment_sop("forge-stale-ledger")])
        .with_capabilities(forge_comment_registry(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-stale-ledger", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "run must park at the first checkpoint: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();

    let first_outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("first checkpoint approve resolves");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(parked_at_second),
    ) = first_outcome
    else {
        panic!("expected first checkpoint to resume into second gate, got {first_outcome:?}");
    };
    assert!(
        matches!(*parked_at_second, SopRunAction::CheckpointWait { .. }),
        "first approval should drive the noop bridge and park at checkpoint 3: {parked_at_second:?}"
    );

    let final_action = engine
        .decide_checkpoint(&run_id, super::super::approval::ApprovalDecision::Approve)
        .expect("direct second checkpoint approval should resume into guarded forge step");
    assert!(
        matches!(final_action, SopRunAction::Failed { .. }),
        "unaudited second checkpoint must fail before forge.comment, got {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "stale checkpoint-1 ledger row must not authorize checkpoint-3 forge write"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().any(|event| {
            event.kind == "gate_resolved"
                && event.payload.get("step").and_then(|value| value.as_u64()) == Some(1)
        }),
        "first checkpoint must write the audited ledger row: {events:?}"
    );
    assert!(
        !events.iter().any(|event| {
            event.kind == "gate_resolved"
                && event.payload.get("step").and_then(|value| value.as_u64()) == Some(3)
        }),
        "direct checkpoint approval must not synthesize a ledger row for step 3: {events:?}"
    );
    let run = engine
        .last_finished_run("forge-stale-ledger")
        .expect("failed run should be retained");
    let result = run
        .step_results
        .iter()
        .find(|result| result.step_number == 4)
        .expect("forge step result recorded");
    assert_eq!(result.status, SopStepStatus::Failed);
    assert!(
        result.output.contains("immediately preceding checkpoint"),
        "failure should name the missing checkpoint-specific ledger row: {result:?}"
    );
}

#[test]
fn forge_comment_rejects_stale_ledger_from_prior_visit_of_same_checkpoint() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = engine_with_sops(vec![same_step_revisit_forge_comment_sop(
        "forge-same-step-revisit",
    )])
    .with_capabilities(forge_comment_registry_with_mutator(Arc::clone(&calls)));

    let first = engine
        .start_run("forge-same-step-revisit", forge_comment_event())
        .unwrap();
    assert!(
        matches!(first, SopRunAction::CheckpointWait { .. }),
        "run must park at the first checkpoint visit: {first:?}"
    );
    let run_id = extract_run_id(&first).to_string();
    assert_eq!(
        engine.get_run(&run_id).map(|run| run.revision),
        Some(0),
        "first checkpoint presentation starts at revision 0"
    );

    let first_outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("first checkpoint approve resolves");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(second_visit),
    ) = first_outcome
    else {
        panic!("expected first checkpoint to resume into second visit, got {first_outcome:?}");
    };
    assert!(
        matches!(*second_visit, SopRunAction::CheckpointWait { .. }),
        "first approval should loop back and park at checkpoint step 1 again: {second_visit:?}"
    );
    assert_eq!(
        engine.get_run(&run_id).map(|run| run.revision),
        Some(1),
        "same-step checkpoint revisit must carry a fresh revision"
    );

    let final_action = engine
        .decide_checkpoint(&run_id, super::super::approval::ApprovalDecision::Approve)
        .expect("direct second checkpoint approval should resume into guarded forge step");
    assert!(
        matches!(final_action, SopRunAction::Failed { .. }),
        "direct second visit approval must not reuse the revision-0 ledger row: {final_action:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "stale first-visit ledger row must not authorize the second-visit forge write"
    );

    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().any(|event| {
            event.kind == "gate_resolved"
                && event.payload.get("step").and_then(|value| value.as_u64()) == Some(1)
                && event
                    .payload
                    .get("checkpoint_revision")
                    .and_then(|value| value.as_u64())
                    == Some(0)
        }),
        "first visit must write the revision-0 ledger row: {events:?}"
    );
    assert!(
        !events.iter().any(|event| {
            event.kind == "gate_resolved"
                && event.payload.get("step").and_then(|value| value.as_u64()) == Some(1)
                && event
                    .payload
                    .get("checkpoint_revision")
                    .and_then(|value| value.as_u64())
                    == Some(1)
        }),
        "direct second visit approval must not synthesize a revision-1 ledger row: {events:?}"
    );
    let run = engine
        .last_finished_run("forge-same-step-revisit")
        .expect("failed run should be retained");
    let result = run
        .step_results
        .iter()
        .find(|result| result.step_number == 3)
        .expect("forge step result recorded");
    assert_eq!(result.status, SopStepStatus::Failed);
    assert!(
        result.output.contains("immediately preceding checkpoint"),
        "failure should name the checkpoint authorization invariant: {result:?}"
    );
}

#[test]
fn resolve_via_broker_approves_checkpoint_and_drives_capability_tail() {
    // The checkpoint bridge (B3): an out-of-band approve of a PausedCheckpoint
    // through the chokepoint must (a) write the audit ledger row, (b) resume via
    // approve_step, and (c) DRIVE the post-checkpoint capability steps
    // headlessly to completion - no live agent turn involved.
    let mut engine = engine_with_sops(vec![capability_checkpoint_sop("cp-tail")]);
    let first = engine.start_run("cp-tail", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let parked = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("drive to the checkpoint");
    assert!(
        matches!(parked, SopRunAction::CheckpointWait { .. }),
        "run must park at the step-2 checkpoint: {parked:?}"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint approve resolves");
    let super::super::approval::BrokerOutcome::Resolved(
        super::super::approval::ResolveOutcome::Resumed(final_action),
    ) = outcome
    else {
        panic!("expected Resolved(Resumed), got {outcome:?}");
    };
    assert!(
        matches!(*final_action, SopRunAction::Completed { .. }),
        "the capability tail must run to completion headlessly: {final_action:?}"
    );
    let run = engine
        .last_finished_run("cp-tail")
        .expect("run reached the finished list");
    assert_eq!(run.status, SopRunStatus::Completed);
    assert_eq!(
        run.step_results.len(),
        3,
        "all three steps (cap, checkpoint, cap) recorded results"
    );
    // The resolution is ledger-audited like any approval gate.
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().any(|ev| ev.kind == "gate_resolved"),
        "checkpoint resolution must append a gate_resolved ledger row: {events:?}"
    );
}

#[test]
fn deterministic_start_pipes_the_trigger_payload_into_step_one() {
    // Regression: `start_deterministic_run` hardcoded step 1's input to Null,
    // so a channel-triggered pipeline's first step never saw the event that
    // triggered it (a triage step received `null` instead of the issue). The
    // start path must apply the same step-1 = trigger-payload mapping as
    // `step_input_value` on the resume/retry paths.
    // `deterministic_sop` has an Execute-kind step 1, whose start action
    // carries the input (a capability step 1 would execute inline instead).
    let mut engine = engine_with_sops(vec![deterministic_sop("det-payload")]);
    let event = SopEvent {
        source: SopTriggerSource::Channel,
        topic: Some("git.main:issues.opened".into()),
        payload: Some(r#"{"repo":"o/r","number":12}"#.into()),
        timestamp: now_iso8601(),
    };
    let first = engine.start_run("det-payload", event).unwrap();
    match &first {
        SopRunAction::DeterministicStep { step, input, .. } => {
            assert_eq!(step.number, 1);
            assert_eq!(
                input,
                &serde_json::json!({"repo": "o/r", "number": 12}),
                "step 1 must receive the parsed trigger payload, not Null"
            );
        }
        other => panic!("expected the step-1 DeterministicStep, got {other:?}"),
    }
}

#[test]
fn resolve_via_broker_denies_checkpoint_through_failure_route() {
    // Deny of a parked checkpoint through the chokepoint follows the authored
    // failure route, records the reason, and audits the resolution. With the
    // default failure route, the run terminates as Failed. Previously a
    // checkpoint could not be denied out-of-band at all (the surfaces returned
    // not_waiting).
    let mut engine = engine_with_sops(vec![capability_checkpoint_sop("cp-deny")]);
    let first = engine.start_run("cp-deny", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let parked = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("drive to the checkpoint");
    assert!(matches!(parked, SopRunAction::CheckpointWait { .. }));

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Deny {
                reason: Some("not appropriate".into()),
            },
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint deny resolves");
    assert!(
        matches!(
            outcome,
            super::super::approval::BrokerOutcome::Resolved(
                super::super::approval::ResolveOutcome::Resumed(_)
            )
        ),
        "expected Resolved(Resumed), got {outcome:?}"
    );
    let run = engine
        .last_finished_run("cp-deny")
        .expect("denied run reached the finished list");
    assert_eq!(run.status, SopRunStatus::Failed);
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().any(|ev| ev.kind == "gate_resolved"),
        "checkpoint deny must append a gate_resolved ledger row: {events:?}"
    );
}

/// `capability(noop) -> checkpoint(edit: body) -> capability(noop)`: the
/// operator-amendable review-gate shape.
fn editable_checkpoint_sop(name: &str) -> Sop {
    let mut sop = capability_checkpoint_sop(name);
    sop.steps[1].edit = Some("body".into());
    sop
}

fn payload_event(payload: &str) -> SopEvent {
    SopEvent {
        source: SopTriggerSource::Manual,
        topic: None,
        payload: Some(payload.into()),
        timestamp: now_iso8601(),
    }
}

#[test]
fn resolve_via_broker_amends_checkpoint_and_pipes_the_edited_field() {
    // An Amend IS an approval of the operator's text: the edited field must
    // replace its counterpart in the piped value, become the checkpoint's
    // recorded output, and flow into the post-checkpoint capability tail —
    // while the predecessor step keeps the model's original for audit.
    let mut engine = engine_with_sops(vec![editable_checkpoint_sop("cp-amend")]);
    let first = engine
        .start_run(
            "cp-amend",
            payload_event(r#"{"body":"model draft","repo":"o/r"}"#),
        )
        .unwrap();
    let run_id = extract_run_id(&first).to_string();
    let parked = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("drive to the checkpoint");
    assert!(matches!(parked, SopRunAction::CheckpointWait { .. }));

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Amend {
                text: "the operator rewrite".into(),
            },
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint amend resolves");
    assert!(
        matches!(
            outcome,
            super::super::approval::BrokerOutcome::Resolved(
                super::super::approval::ResolveOutcome::Resumed(_)
            )
        ),
        "expected Resolved(Resumed), got {outcome:?}"
    );
    let run = engine
        .last_finished_run("cp-amend")
        .expect("amended run completed");
    assert_eq!(run.status, SopRunStatus::Completed);
    // Step 1 keeps the model's original.
    let step1: serde_json::Value = serde_json::from_str(
        &run.step_results
            .iter()
            .find(|r| r.step_number == 1)
            .unwrap()
            .output,
    )
    .unwrap();
    assert_eq!(step1["body"], "model draft");
    // The checkpoint's output AND the tail step's input carry the rewrite,
    // with the untouched fields intact.
    for step_number in [2u32, 3] {
        let out: serde_json::Value = serde_json::from_str(
            &run.step_results
                .iter()
                .find(|r| r.step_number == step_number)
                .unwrap()
                .output,
        )
        .unwrap();
        assert_eq!(
            out["body"], "the operator rewrite",
            "step {step_number} must carry the amended body"
        );
        assert_eq!(out["repo"], "o/r", "unedited fields must survive");
    }
    // The ledger records the resolution as an amend.
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events
            .iter()
            .any(|ev| ev.kind == "gate_resolved" && ev.payload["decision"] == "amend"),
        "amend must append a decision=amend ledger row: {events:?}"
    );
}

#[test]
fn amend_without_a_declared_edit_field_fails_closed() {
    // No `- edit:` on the checkpoint → an Amend must be refused BEFORE any
    // ledger row or run mutation, leaving the gate parked and answerable.
    let mut engine = engine_with_sops(vec![capability_checkpoint_sop("cp-noedit")]);
    let first = engine.start_run("cp-noedit", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();

    let res = engine.resolve_via_broker(
        &run_id,
        super::super::approval::ApprovalDecision::Amend { text: "x".into() },
        super::super::approval::ApprovalPrincipal::cli(None),
    );
    assert!(res.is_err(), "amend without `- edit:` must fail closed");
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "the gate must stay parked"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().all(|ev| ev.kind != "gate_resolved"),
        "a refused amend must not leave a gate_resolved row: {events:?}"
    );
}

/// A stub `llm.generate` that bakes the reviewer feedback into its output,
/// so a revise's re-draft is distinguishable from the original.
struct StubLlmGenerate;

impl super::super::capability::SopCapability for StubLlmGenerate {
    fn id(&self) -> &'static str {
        "llm.generate"
    }
    fn describe(&self) -> super::super::capability::CapabilityInfo {
        super::super::capability::CapabilityInfo {
            id: self.id(),
            description: "stub llm.generate",
            deterministic: true,
            idempotent: false,
            reversible: true,
            supports_retry: true,
            required_permissions: vec![],
            input_schema: None,
            output_schema: None,
        }
    }
    fn execute(
        &self,
        _ctx: super::super::capability::CapabilityContext,
        input: serde_json::Value,
    ) -> Result<super::super::capability::CapabilityResult> {
        let feedback = input
            .get("revision_feedback")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        Ok(super::super::capability::CapabilityResult::success(
            serde_json::json!({"body": format!("draft [feedback: {feedback}]")}),
        ))
    }
}

/// A stub `llm.generate` that succeeds on the FIRST draft (no
/// `revision_feedback`) but fails on the RE-draft — so the run reaches the
/// checkpoint normally, and only the Revise re-run models a provider outage.
struct FailsOnlyOnRevise;

impl super::super::capability::SopCapability for FailsOnlyOnRevise {
    fn id(&self) -> &'static str {
        "llm.generate"
    }
    fn describe(&self) -> super::super::capability::CapabilityInfo {
        super::super::capability::CapabilityInfo {
            id: self.id(),
            description: "stub llm.generate that fails only on re-draft",
            deterministic: true,
            idempotent: false,
            reversible: true,
            supports_retry: true,
            required_permissions: vec![],
            input_schema: None,
            output_schema: None,
        }
    }
    fn execute(
        &self,
        _ctx: super::super::capability::CapabilityContext,
        input: serde_json::Value,
    ) -> Result<super::super::capability::CapabilityResult> {
        if input.get("revision_feedback").is_some() {
            Ok(super::super::capability::CapabilityResult::failure(
                "model provider unavailable",
            ))
        } else {
            Ok(super::super::capability::CapabilityResult::success(
                serde_json::json!({"body": "original draft"}),
            ))
        }
    }
}

/// `capability(llm.generate stub) -> checkpoint`: the revisable review-gate
/// shape, with the stub registered over the fail-closed builtin.
fn revisable_checkpoint_engine(name: &str) -> SopEngine {
    let mut sop = capability_checkpoint_sop(name);
    sop.steps[0].capability = Some("llm.generate".into());
    sop.steps.truncate(2);
    let mut registry = super::super::capability::SopCapabilityRegistry::with_builtins();
    registry.register(StubLlmGenerate);
    engine_with_sops(vec![sop]).with_capabilities(Arc::new(registry))
}

#[test]
fn failed_revise_writes_no_resolved_row_and_leaves_the_draft_unchanged() {
    // The resolved ledger row must not be appended before the re-draft's
    // fallible model call. A failed Revise leaves zero gate_resolved rows, the
    // original draft parked, and the revision counter untouched.
    let mut sop = capability_checkpoint_sop("cp-revise-fail");
    sop.steps[0].capability = Some("llm.generate".into());
    sop.steps.truncate(2);
    let mut registry = super::super::capability::SopCapabilityRegistry::with_builtins();
    registry.register(FailsOnlyOnRevise);
    let mut engine = engine_with_sops(vec![sop]).with_capabilities(Arc::new(registry));

    let first = engine.start_run("cp-revise-fail", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();
    let original_draft = engine
        .get_run(&run_id)
        .unwrap()
        .step_results
        .iter()
        .find(|r| r.step_number == 1)
        .unwrap()
        .output
        .clone();

    let res = engine.resolve_via_broker(
        &run_id,
        super::super::approval::ApprovalDecision::Revise {
            guidance: "make it shorter".into(),
        },
        super::super::approval::ApprovalPrincipal::cli(None),
    );
    assert!(res.is_err(), "a failed re-draft must surface an error");

    let run = engine.get_run(&run_id).expect("run stays parked");
    assert_eq!(run.status, SopRunStatus::PausedCheckpoint);
    assert_eq!(
        run.revision, 0,
        "a failed revise must not bump the revision"
    );
    assert_eq!(
        run.step_results
            .iter()
            .find(|r| r.step_number == 1)
            .unwrap()
            .output,
        original_draft,
        "the original draft must remain untouched"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events.iter().all(|ev| ev.kind != "gate_resolved"),
        "a failed revise must leave NO gate_resolved ledger row: {events:?}"
    );

    // The gate is still answerable: the run must admit a fresh exec claim
    // (a leaked claim from the failed revise would block this deny).
    engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Deny { reason: None },
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("the gate is still resolvable after a failed revise");
    assert_eq!(
        engine.last_finished_run("cp-revise-fail").unwrap().status,
        SopRunStatus::Failed
    );
}

#[test]
fn resolve_via_broker_revises_checkpoint_and_represents_the_gate() {
    let mut engine = revisable_checkpoint_engine("cp-revise");
    let first = engine.start_run("cp-revise", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let parked = engine
        .drive_headless_deterministic(&run_id, first)
        .expect("drive to the checkpoint");
    assert!(matches!(parked, SopRunAction::CheckpointWait { .. }));

    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Revise {
                guidance: "make it shorter".into(),
            },
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("checkpoint revise resolves");
    assert!(
        matches!(
            outcome,
            super::super::approval::BrokerOutcome::Resolved(
                super::super::approval::ResolveOutcome::Revised
            )
        ),
        "expected Resolved(Revised), got {outcome:?}"
    );
    // The run never left the gate; the draft was replaced and the gate
    // revision bumped so the old prompt's reference is superseded.
    let run = engine.get_run(&run_id).expect("run still active");
    assert_eq!(run.status, SopRunStatus::PausedCheckpoint);
    assert_eq!(run.revision, 1);
    let redraft = &run
        .step_results
        .iter()
        .find(|r| r.step_number == 1)
        .unwrap()
        .output;
    assert!(
        redraft.contains("make it shorter"),
        "the re-draft must reflect the guidance: {redraft}"
    );
    let events = engine.run_events(&run_id).unwrap_or_default();
    assert!(
        events
            .iter()
            .any(|ev| ev.kind == "gate_resolved" && ev.payload["decision"] == "revise"),
        "revise must append a decision=revise ledger row: {events:?}"
    );

    // The revised gate is still answerable: approve completes with the NEW
    // draft as the checkpoint's output.
    let outcome = engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .expect("revised gate approves");
    assert!(matches!(
        outcome,
        super::super::approval::BrokerOutcome::Resolved(
            super::super::approval::ResolveOutcome::Resumed(_)
        )
    ));
    let finished = engine.last_finished_run("cp-revise").unwrap();
    assert_eq!(finished.status, SopRunStatus::Completed);
}

#[test]
fn revise_is_capped_and_refuses_on_a_non_llm_predecessor() {
    // Cap: MAX_GATE_REVISIONS re-drafts, then fail closed (bounded spend).
    let mut engine = revisable_checkpoint_engine("cp-revise-cap");
    let first = engine.start_run("cp-revise-cap", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();
    for i in 1..=MAX_GATE_REVISIONS {
        engine
            .resolve_via_broker(
                &run_id,
                super::super::approval::ApprovalDecision::Revise {
                    guidance: format!("round {i}"),
                },
                super::super::approval::ApprovalPrincipal::cli(None),
            )
            .unwrap_or_else(|e| panic!("revision {i} within the cap must resolve: {e}"));
    }
    let res = engine.resolve_via_broker(
        &run_id,
        super::super::approval::ApprovalDecision::Revise {
            guidance: "one too many".into(),
        },
        super::super::approval::ApprovalPrincipal::cli(None),
    );
    assert!(
        res.is_err(),
        "the revision cap must refuse further re-drafts"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().revision,
        MAX_GATE_REVISIONS
    );

    // A noop predecessor is not revisable at all (nothing to re-draft).
    let mut engine = engine_with_sops(vec![capability_checkpoint_sop("cp-norevise")]);
    let first = engine.start_run("cp-norevise", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();
    let res = engine.resolve_via_broker(
        &run_id,
        super::super::approval::ApprovalDecision::Revise {
            guidance: "shorter".into(),
        },
        super::super::approval::ApprovalPrincipal::cli(None),
    );
    assert!(
        res.is_err(),
        "a gate without an llm.generate predecessor must refuse revise"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );
}

#[test]
fn gate_presentations_get_unique_revisions_and_a_per_gate_revise_budget() {
    // llm -> checkpoint -> llm -> checkpoint. Every gate presentation the
    // run ever makes must carry a UNIQUE revision (so a stale earlier-gate
    // prompt can never resolve a later gate), and the revise cap must be a
    // per-GATE budget, not a run-wide one.
    let mut sop = capability_checkpoint_sop("cp-two-gates");
    sop.steps[0].capability = Some("llm.generate".into());
    sop.steps[2].capability = Some("llm.generate".into());
    sop.steps.push(SopStep {
        number: 4,
        title: "Gate 2".into(),
        kind: SopStepKind::Checkpoint,
        ..SopStep::default()
    });
    let mut registry = super::super::capability::SopCapabilityRegistry::with_builtins();
    registry.register(StubLlmGenerate);
    let mut engine = engine_with_sops(vec![sop]).with_capabilities(Arc::new(registry));

    let first = engine.start_run("cp-two-gates", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();
    {
        let run = engine.get_run(&run_id).unwrap();
        assert_eq!(run.current_step, 2, "parked at gate 1");
        assert_eq!(run.revision, 0, "the run's first gate is revision 0");
        assert_eq!(run.revision_base, 0);
    }
    // One revise at gate 1: revision 1.
    engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Revise {
                guidance: "shorter".into(),
            },
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert_eq!(engine.get_run(&run_id).unwrap().revision, 1);
    // Approve gate 1 -> the tail drives to gate 2, whose presentation must
    // be revision 2 (unique vs gate 1's 0 and 1) with a FRESH revise budget.
    engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .unwrap();
    {
        let run = engine.get_run(&run_id).unwrap();
        assert_eq!(run.status, SopRunStatus::PausedCheckpoint);
        assert_eq!(run.current_step, 4, "parked at gate 2");
        assert_eq!(
            run.revision, 2,
            "a new gate presentation bumps past every earlier reference"
        );
        assert_eq!(run.revision_base, 2, "the revise budget rebases per gate");
    }
    // Gate 2 has its FULL budget despite gate 1's spend.
    for i in 1..=MAX_GATE_REVISIONS {
        engine
            .resolve_via_broker(
                &run_id,
                super::super::approval::ApprovalDecision::Revise {
                    guidance: format!("gate2 round {i}"),
                },
                super::super::approval::ApprovalPrincipal::cli(None),
            )
            .unwrap_or_else(|e| panic!("gate 2 revision {i} within its own budget: {e}"));
    }
    assert!(
        engine
            .resolve_via_broker(
                &run_id,
                super::super::approval::ApprovalDecision::Revise {
                    guidance: "over budget".into(),
                },
                super::super::approval::ApprovalPrincipal::cli(None),
            )
            .is_err(),
        "gate 2's own cap must still bound spend"
    );
}

#[test]
fn terminal_run_removes_the_park_snapshot_file() {
    // Fix 0b: a resolved run must not leave a stale `<run_id>.state.json`
    // claiming it is still paused — the run store and approval ledger are
    // the durable record, the snapshot is only a rehydration artifact.
    let dir = std::env::temp_dir().join(format!("zc-snapshot-cleanup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut sop = capability_checkpoint_sop("cp-snapshot");
    sop.location = Some(dir.clone());
    let mut engine = engine_with_sops(vec![sop]);

    // Denied run: snapshot written at park, gone after the deny.
    let first = engine.start_run("cp-snapshot", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();
    let state_file = dir.join(format!("{run_id}.state.json"));
    assert!(state_file.exists(), "the park must write the snapshot");
    engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Deny { reason: None },
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(
        !state_file.exists(),
        "a terminally denied run must remove its park snapshot"
    );

    // Approved run: snapshot gone after completion too.
    let first = engine.start_run("cp-snapshot", manual_event()).unwrap();
    let run_id = extract_run_id(&first).to_string();
    let _ = engine.drive_headless_deterministic(&run_id, first).unwrap();
    let state_file = dir.join(format!("{run_id}.state.json"));
    assert!(state_file.exists());
    engine
        .resolve_via_broker(
            &run_id,
            super::super::approval::ApprovalDecision::Approve,
            super::super::approval::ApprovalPrincipal::cli(None),
        )
        .unwrap();
    assert!(
        !state_file.exists(),
        "a completed run must remove its park snapshot"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sop_approve_tool_resumes_deterministic_checkpoint() {
    // Regression guard: the sop_approve tool must route a
    // PausedCheckpoint to approve_step, because resolve_gate reports NotWaiting
    // for it. Without that routing the tool answers "not waiting for approval"
    // and a deterministic run is stuck unresumable through every surface.
    use crate::tools::SopApproveTool;
    use zeroclaw_api::tool::Tool;

    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp")]);
    let action = engine.start_run("det-cp", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    let tool = SopApproveTool::new(std::sync::Arc::new(std::sync::Mutex::new(engine)));
    let result = tool
        .execute(serde_json::json!({ "run_id": run_id }))
        .await
        .unwrap();
    assert!(
        result.success,
        "sop_approve must resume a deterministic checkpoint, not report not-waiting: {result:?}"
    );
    assert!(
        result.output.contains("Approved"),
        "checkpoint resume should be reported as approved: {result:?}"
    );
}

#[test]
fn engine_restores_runs_from_store() {
    use super::super::store::SqliteRunStore;
    let path =
        std::env::temp_dir().join(format!("zc-sop-engine-restore-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    // Seed a WaitingApproval run directly into a durable store.
    let store = std::sync::Arc::new(SqliteRunStore::open(&path).unwrap());
    let run = SopRun {
        run_id: "r-restore".to_string(),
        sop_name: "deploy".to_string(),
        trigger_event: SopEvent {
            source: SopTriggerSource::Manual,
            topic: None,
            payload: None,
            timestamp: now_iso8601(),
        },
        frame_marker_id: "marker-restore".to_string(),
        status: SopRunStatus::WaitingApproval,
        current_step: 1,
        total_steps: 2,
        started_at: now_iso8601(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: Some(now_iso8601()),
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    store
        .save_run(&PersistedRun::new(
            run,
            now_iso8601(),
            SopTriggerSource::Manual,
        ))
        .unwrap();
    // A fresh engine wired to the same store rehydrates the run on boot.
    let mut engine = SopEngine::new(SopConfig::default()).with_store(store);
    engine.restore_runs();
    assert!(engine.active_runs().contains_key("r-restore"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn engine_persist_bumps_revision_across_active_and_terminal() {
    use super::super::store::SqliteRunStore;
    let path =
        std::env::temp_dir().join(format!("zc-sop-engine-persist-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = std::sync::Arc::new(SqliteRunStore::open(&path).unwrap());
    let mut engine = SopEngine::new(SopConfig::default()).with_store(store.clone());

    let mut run = SopRun {
        run_id: "r-persist".to_string(),
        sop_name: "deploy".to_string(),
        trigger_event: SopEvent {
            source: SopTriggerSource::Manual,
            topic: None,
            payload: None,
            timestamp: now_iso8601(),
        },
        frame_marker_id: "marker-persist".to_string(),
        status: SopRunStatus::Running,
        current_step: 0,
        total_steps: 2,
        started_at: now_iso8601(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: None,
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    engine.active_runs.insert(run.run_id.clone(), run.clone());

    // First persist lands at revision 0.
    engine.persist_active("r-persist");
    assert_eq!(store.load_run("r-persist").unwrap().unwrap().revision, 0);

    // Advancing the run and persisting again is a divergent state at the next
    // revision. The old revision-0-always wiring would have had this rejected
    // as a same-revision conflict and silently kept the stale snapshot.
    run.current_step = 1;
    engine.active_runs.insert(run.run_id.clone(), run.clone());
    engine.persist_active("r-persist");
    let after = store.load_run("r-persist").unwrap().unwrap();
    assert_eq!(after.revision, 1);
    assert_eq!(after.run.current_step, 1, "latest state persisted");

    // The terminal write advances again, is accepted, and leaves no active run.
    run.status = SopRunStatus::Completed;
    run.completed_at = Some(now_iso8601());
    engine.persist_terminal(&run).unwrap();
    assert!(
        store.load_active_runs().unwrap().is_empty(),
        "terminal excluded from active"
    );
    assert_eq!(store.load_run("r-persist").unwrap().unwrap().revision, 2);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn deterministic_active_run_persists_and_restores_before_terminal() {
    use super::super::store::SqliteRunStore;
    let path = std::env::temp_dir().join(format!("zc-sop-det-restore-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = std::sync::Arc::new(SqliteRunStore::open(&path).unwrap());

    let mut engine = SopEngine::new(SopConfig::default()).with_store(store.clone());
    engine.set_sops_for_test(vec![deterministic_sop("det-sop")]);

    // Start: the first deterministic step (Running) must be persisted as active,
    // not only on terminal completion.
    let action = engine.start_run("det-sop", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();
    let active = store.load_active_runs().unwrap();
    assert_eq!(
        active.len(),
        1,
        "deterministic start must persist an active run"
    );
    assert_eq!(active[0].run.run_id, run_id);
    assert_eq!(active[0].run.current_step, 1);

    // Advance into the checkpoint: still non-terminal, must stay persisted in
    // the shared store (not only in the deterministic state file).
    let action = engine
        .advance_deterministic_step(&run_id, serde_json::json!({"r": 1}), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));
    let stored = store.load_run(&run_id).unwrap().unwrap();
    assert_eq!(stored.run.current_step, 2);
    assert_eq!(stored.run.status, SopRunStatus::PausedCheckpoint);

    // Simulate a daemon restart mid-run: a fresh engine on the same store must
    // rehydrate the in-flight deterministic run (the gap this fixes).
    let mut restarted = SopEngine::new(SopConfig::default()).with_store(store.clone());
    restarted.restore_runs();
    assert!(
        restarted.active_runs().contains_key(&run_id),
        "deterministic in-flight run must rehydrate after restart"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn restored_policied_checkpoint_replays_request_route() {
    use zeroclaw_config::schema::ApprovalPolicyConfig;

    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let adapter = std::sync::Arc::new(RecordingRouteAdapter {
        calls: calls.clone(),
    });
    let broker = std::sync::Arc::new(crate::sop::approval::ApprovalBroker::with_route(adapter));
    let mut config = SopConfig::default();
    config.approval.policies.insert(
        "prod".to_string(),
        ApprovalPolicyConfig {
            required_group: None,
            quorum: 0,
            request_route: Some("discord.ops:123456789".to_string()),
            escalation_route: None,
        },
    );
    let mut sop = deterministic_sop("det-restore-route");
    sop.steps[1].policy = Some("prod".to_string());

    let mut source = engine_with_config_sops(config.clone(), vec![sop.clone()])
        .with_store(store.clone())
        .with_approval_broker(broker.clone());
    let action = source
        .start_run("det-restore-route", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    let action = source
        .advance_deterministic_step(&run_id, serde_json::json!({"step": 1}), None)
        .unwrap();
    assert!(matches!(action, SopRunAction::CheckpointWait { .. }));
    assert_eq!(calls.lock().unwrap().len(), 1, "initial park delivers once");

    // Model a daemon exit after persistence but before the external adapter's
    // fire-and-forget delivery completes. Only the restored engine may send.
    calls.lock().unwrap().clear();
    let mut restarted = engine_with_config_sops(config, vec![sop])
        .with_store(store)
        .with_approval_broker(broker);
    restarted.restore_runs();

    assert_eq!(
        restarted.get_run(&run_id).map(|run| run.status),
        Some(SopRunStatus::PausedCheckpoint)
    );
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [(
            crate::sop::approval::ApprovalNoticeKind::Request,
            "discord.ops:123456789".to_string(),
            run_id,
            "det-restore-route".to_string(),
            2
        )],
        "restore replays the persisted checkpoint through its request route"
    );
}

#[test]
fn deny_checkpoint_goto_continuation_respects_per_sop_cap() {
    // A denied checkpoint whose `on_failure = Goto` CONTINUES execution, so it must
    // pass the same capped store CAS as every other resume-to-continue path. With
    // max_concurrent = 1 and the slot already taken, denying a parked checkpoint
    // returns typed backpressure and leaves it parked - it does NOT resume above cap.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = deterministic_sop("det-cp");
    sop.max_concurrent = 1;
    sop.steps[1].on_failure = StepFailure::Goto { step: 3 };
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());

    let a = engine.start_run("det-cp", manual_event()).unwrap();
    let id_a = extract_run_id(&a).to_string();
    engine
        .advance_deterministic_step(&id_a, serde_json::json!("a1"), None)
        .unwrap();
    let b = engine.start_run("det-cp", manual_event()).unwrap();
    let id_b = extract_run_id(&b).to_string();
    engine
        .advance_deterministic_step(&id_b, serde_json::json!("b1"), None)
        .unwrap();
    assert_eq!(
        store.claim_counts("det-cp").unwrap(),
        (0, 0),
        "both parked at the checkpoint: no exec claim held"
    );

    // Approve A -> it takes the one slot.
    engine.approve_step(&id_a).unwrap();
    assert_eq!(
        store.claim_counts("det-cp").unwrap().0,
        1,
        "A holds the one slot"
    );

    // Deny B's checkpoint: its Goto continuation must be refused at capacity.
    let err = engine
        .decide_checkpoint(
            &id_b,
            ApprovalDecision::Deny {
                reason: Some("nope".into()),
            },
        )
        .expect_err("a denied Goto continuation must be refused at capacity");
    assert!(
        err_is_resume_at_capacity(&err),
        "the refusal is typed capacity backpressure, not a fault: {err}"
    );
    assert_eq!(
        engine.get_run(&id_b).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "B stays paused at the checkpoint, re-resolvable"
    );
    assert_eq!(
        store.claim_counts("det-cp").unwrap().0,
        1,
        "still exactly one slot in use, not two"
    );
}

#[test]
fn deny_checkpoint_retry_continuation_respects_global_cap() {
    // A denied checkpoint whose `on_failure = Retry` (budget remaining) CONTINUES,
    // so it is capped against the GLOBAL limit too. Two SOPs share
    // max_concurrent_total = 1; with the one global slot taken, denying a parked
    // checkpoint on the other returns typed backpressure and stays parked. A
    // terminal denial (Fail, or Retry exhausted) would instead stay uncapped.
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut s1 = deterministic_sop("det-a");
    s1.max_concurrent = 1;
    let mut s2 = deterministic_sop("det-b");
    s2.max_concurrent = 1;
    s2.steps[1].on_failure = StepFailure::Retry { max: 3 };
    let cfg = SopConfig {
        max_concurrent_total: 1,
        ..SopConfig::default()
    };
    let mut engine = engine_with_config_sops(cfg, vec![s1, s2]).with_store(store.clone());

    let a = engine.start_run("det-a", manual_event()).unwrap();
    let id_a = extract_run_id(&a).to_string();
    engine
        .advance_deterministic_step(&id_a, serde_json::json!("a1"), None)
        .unwrap();
    let b = engine.start_run("det-b", manual_event()).unwrap();
    let id_b = extract_run_id(&b).to_string();
    engine
        .advance_deterministic_step(&id_b, serde_json::json!("b1"), None)
        .unwrap();

    // Approve det-a -> it takes the one global slot.
    engine.approve_step(&id_a).unwrap();
    assert_eq!(
        store.claim_counts("det-a").unwrap().1,
        1,
        "the one global slot is taken"
    );

    // Deny det-b's checkpoint: its Retry continuation is refused at the global cap.
    let err = engine
        .decide_checkpoint(
            &id_b,
            ApprovalDecision::Deny {
                reason: Some("nope".into()),
            },
        )
        .expect_err("a denied Retry continuation must be refused at the global cap");
    assert!(
        err_is_resume_at_capacity(&err),
        "the refusal is typed capacity backpressure: {err}"
    );
    assert_eq!(
        engine.get_run(&id_b).unwrap().status,
        SopRunStatus::PausedCheckpoint,
        "det-b stays paused, re-resolvable"
    );
    assert_eq!(
        store.claim_counts("det-b").unwrap().1,
        1,
        "still exactly one global slot in use, not two"
    );
}

#[test]
fn deny_checkpoint_routes_through_on_failure_goto() {
    // A denied checkpoint takes the failure path: the checkpoint step is
    // recorded Failed and routed through its `on_failure`. With a Goto, the
    // run continues at the authored failure-handler step, not the success
    // successor and not a whole-run cancel.
    let mut sop = deterministic_sop("det-cp-deny-goto");
    sop.steps[1].on_failure = StepFailure::Goto { step: 3 };
    let mut engine = engine_with_sops(vec![sop]);
    let action = engine
        .start_run("det-cp-deny-goto", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();

    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    let action = engine
        .decide_checkpoint(
            &run_id,
            ApprovalDecision::Deny {
                reason: Some("not acceptable".into()),
            },
        )
        .unwrap();
    assert!(
        matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 3),
        "denying a checkpoint with on_failure=Goto must route to the failure-handler step"
    );
    let cp = engine
        .get_run(&run_id)
        .unwrap()
        .step_results
        .iter()
        .find(|r| r.step_number == 2)
        .expect("checkpoint step recorded");
    assert_eq!(cp.status, SopStepStatus::Failed);
}

#[test]
fn deny_checkpoint_goto_rolls_back_when_active_save_fails() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let mut sop = deterministic_sop("det-cp-deny-goto-save-fail");
    sop.steps[1].on_failure = StepFailure::Goto { step: 3 };
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-goto-save-fail", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();

    let before = engine.get_run(&run_id).unwrap();
    let prior_waiting_since = before.waiting_since.clone();
    let prior_step_results = before.step_results.len();
    let prior_current_step = before.current_step;
    assert_eq!(
        store.claim_counts("det-cp-deny-goto-save-fail").unwrap(),
        (0, 0),
        "the checkpoint must be durably parked before the save failure is injected"
    );

    store
        .fail_save
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("active save failure must reject the denied checkpoint transition");
    assert!(
        err.to_string()
            .contains("failed to persist checkpoint denial transition"),
        "unexpected error: {err}"
    );

    let restored = engine.get_run(&run_id).unwrap();
    assert_eq!(restored.status, SopRunStatus::PausedCheckpoint);
    assert_eq!(restored.current_step, prior_current_step);
    assert_eq!(restored.waiting_since, prior_waiting_since);
    assert_eq!(restored.step_results.len(), prior_step_results);
    assert_eq!(
        store.claim_counts("det-cp-deny-goto-save-fail").unwrap(),
        (0, 0),
        "the claim reacquired for the rejected denial must be released"
    );
    let events = store.list_events(&run_id).unwrap();
    assert!(
        !events.iter().any(|event| event.kind == "checkpoint_denied"),
        "a failed denied-checkpoint transition must not emit checkpoint_denied: {events:?}"
    );
}

#[test]
fn deny_checkpoint_retry_rolls_back_when_active_save_fails() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let mut sop = deterministic_sop("det-cp-deny-retry-save-fail");
    sop.steps[1].on_failure = StepFailure::Retry { max: 2 };
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-retry-save-fail", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();

    let before = engine.get_run(&run_id).unwrap();
    let prior_waiting_since = before.waiting_since.clone();
    let prior_step_results = before.step_results.len();
    let prior_current_step = before.current_step;
    assert_eq!(
        store.claim_counts("det-cp-deny-retry-save-fail").unwrap(),
        (0, 0),
        "the checkpoint must be durably parked before the save failure is injected"
    );

    store
        .fail_save
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("active save failure must reject the denied checkpoint retry");
    assert!(
        err.to_string()
            .contains("failed to persist checkpoint denial transition"),
        "unexpected error: {err}"
    );

    let restored = engine.get_run(&run_id).unwrap();
    assert_eq!(restored.status, SopRunStatus::PausedCheckpoint);
    assert_eq!(restored.current_step, prior_current_step);
    assert_eq!(restored.waiting_since, prior_waiting_since);
    assert_eq!(restored.step_results.len(), prior_step_results);
    assert_eq!(
        store.claim_counts("det-cp-deny-retry-save-fail").unwrap(),
        (0, 0),
        "the claim reacquired for the rejected retry denial must be released"
    );
    let events = store.list_events(&run_id).unwrap();
    assert!(
        !events.iter().any(|event| event.kind == "checkpoint_denied"),
        "a failed denied-checkpoint retry must not emit checkpoint_denied: {events:?}"
    );
}

#[test]
fn deny_checkpoint_defaults_to_terminal_failure() {
    // With the default on_failure (Fail), a denied checkpoint terminates the
    // run Failed. This is distinct from Cancelled: the operator declined and
    // no failure handler was authored, so the run failed.
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp-deny-fail")]);
    let action = engine
        .start_run("det-cp-deny-fail", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();

    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::PausedCheckpoint
    );

    let action = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .unwrap();
    assert!(
        matches!(action, SopRunAction::Failed { .. }),
        "denying a checkpoint with default on_failure must fail the run"
    );
    assert_eq!(
        engine.get_run(&run_id).unwrap().status,
        SopRunStatus::Failed
    );
}

#[test]
fn deny_checkpoint_keeps_claim_when_terminal_persist_fails() {
    let store = std::sync::Arc::new(FailingAppendStore {
        inner: InMemoryRunStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
        fail_save: std::sync::atomic::AtomicBool::new(false),
        fail_finish: std::sync::atomic::AtomicBool::new(false),
    });
    let mut sop = deterministic_sop("det-cp-deny-finish-fail");
    sop.max_concurrent = 1;
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-finish-fail", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();

    let before = engine.get_run(&run_id).unwrap();
    let prior_waiting_since = before.waiting_since.clone();
    let prior_step_results = before.step_results.len();
    let prior_current_step = before.current_step;
    assert_eq!(
        store.claim_counts("det-cp-deny-finish-fail").unwrap(),
        (0, 0),
        "a durably parked checkpoint starts without an execution claim"
    );

    store
        .fail_finish
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("terminal persistence failure must reject the decision");
    assert!(err.to_string().contains("injected finish failure"));

    let restored = engine.get_run(&run_id).unwrap();
    assert_eq!(restored.status, SopRunStatus::PausedCheckpoint);
    assert_eq!(restored.current_step, prior_current_step);
    assert_eq!(restored.waiting_since, prior_waiting_since);
    assert_eq!(restored.step_results.len(), prior_step_results);
    assert_eq!(
        store.claim_counts("det-cp-deny-finish-fail").unwrap(),
        (1, 1),
        "a failed terminal write keeps the reacquired claim fail-closed"
    );
}

#[test]
fn deny_checkpoint_preflights_invalid_failure_goto_without_mutation() {
    let store = std::sync::Arc::new(InMemoryRunStore::new());
    let mut sop = deterministic_sop("det-cp-deny-invalid-goto");
    sop.steps[1].on_failure = StepFailure::Goto { step: 99 };
    let mut engine = engine_with_sops(vec![sop]).with_store(store.clone());
    let action = engine
        .start_run("det-cp-deny-invalid-goto", manual_event())
        .unwrap();
    let run_id = extract_run_id(&action).to_string();
    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();

    let before = engine.get_run(&run_id).unwrap();
    let prior_waiting_since = before.waiting_since.clone();
    let prior_step_results = before.step_results.len();
    let prior_current_step = before.current_step;
    let err = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Deny { reason: None })
        .expect_err("an invalid failure-route target must be rejected before mutation");
    assert!(err.to_string().contains("step 99"));

    let restored = engine.get_run(&run_id).unwrap();
    assert_eq!(restored.status, SopRunStatus::PausedCheckpoint);
    assert_eq!(restored.current_step, prior_current_step);
    assert_eq!(restored.waiting_since, prior_waiting_since);
    assert_eq!(restored.step_results.len(), prior_step_results);
    assert_eq!(
        store.claim_counts("det-cp-deny-invalid-goto").unwrap(),
        (0, 0),
        "preflight must not acquire a claim for an invalid failure route"
    );
    assert!(
        !store
            .list_events(&run_id)
            .unwrap()
            .iter()
            .any(|event| event.kind == "checkpoint_denied"),
        "an invalid route must not leave a denied-checkpoint event behind"
    );
}

#[test]
fn decide_checkpoint_approve_matches_approve_step() {
    // Approve through the unified decision entry point must behave exactly
    // like approve_step: resume to the next step down the success edge.
    let mut engine = engine_with_sops(vec![deterministic_sop("det-cp-approve")]);
    let action = engine.start_run("det-cp-approve", manual_event()).unwrap();
    let run_id = extract_run_id(&action).to_string();

    engine
        .advance_deterministic_step(&run_id, serde_json::json!("s1-out"), None)
        .unwrap();
    let action = engine
        .decide_checkpoint(&run_id, ApprovalDecision::Approve)
        .unwrap();
    assert!(
        matches!(action, SopRunAction::DeterministicStep { ref step, .. } if step.number == 3),
        "approving via decide_checkpoint must resume to the next step"
    );
}

#[test]
fn engine_restores_finished_runs_from_store() {
    use super::super::store::SqliteRunStore;
    let path = std::env::temp_dir().join(format!(
        "zc-sop-engine-restore-fin-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = std::sync::Arc::new(SqliteRunStore::open(&path).unwrap());

    // Persist a terminal run: saved active, then finished with a bumped revision.
    let base = SopRun {
        run_id: "r-done".to_string(),
        sop_name: "deploy".to_string(),
        trigger_event: SopEvent {
            source: SopTriggerSource::Manual,
            topic: None,
            payload: None,
            timestamp: now_iso8601(),
        },
        frame_marker_id: "marker-done".to_string(),
        status: SopRunStatus::Running,
        current_step: 0,
        total_steps: 1,
        started_at: now_iso8601(),
        completed_at: None,
        step_results: Vec::new(),
        waiting_since: None,
        llm_calls_saved: 0,
        revision: 0,
        revision_base: 0,
    };
    store
        .save_run(&PersistedRun::new(
            base.clone(),
            now_iso8601(),
            SopTriggerSource::Manual,
        ))
        .unwrap();
    let mut terminal = base;
    terminal.status = SopRunStatus::Completed;
    terminal.completed_at = Some(now_iso8601());
    let mut persisted = PersistedRun::new(terminal, now_iso8601(), SopTriggerSource::Manual);
    persisted.revision = 1;
    store.finish_run("r-done", &persisted).unwrap();

    // A fresh engine seeds its retention window from the store's terminal set.
    let mut engine = SopEngine::new(SopConfig::default()).with_store(store);
    engine.restore_runs();
    assert!(
        !engine.active_runs().contains_key("r-done"),
        "terminal run must not rehydrate as active"
    );
    let finished = engine.finished_runs(None);
    assert_eq!(
        finished.len(),
        1,
        "terminal run seeded into retention window"
    );
    assert_eq!(finished[0].run_id, "r-done");
    assert_eq!(finished[0].status, SopRunStatus::Completed);
    let _ = std::fs::remove_file(&path);
}
