use std::fs;
use zeroclaw_api::cost::{BudgetCheck, CostRecord, CostSummary, UsagePeriod};
use zeroclaw_config::schema::CostConfig;
use zeroclaw_cost::{CostTracker, calculate_token_usage};

#[test]
fn existing_ledger_and_shared_results_survive_reload_and_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state/costs.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A pre-extraction record with the original persisted task key and no
    // cached-token, pricing-availability or agent-attribution fields.
    let legacy = concat!(
        r#"{"id":"old-record","usage":{"model":"test/model","input_tokens":10,"output_tokens":5,"total_tokens":15,"cost_usd":0.001,"timestamp":"2026-06-30T00:00:00Z"},"session_id":"old-session","goal_task_id":"task-1"}"#,
        "\n"
    );
    fs::write(&path, legacy).unwrap();
    let config = CostConfig {
        enabled: true,
        track_per_agent: true,
        daily_limit_usd: 1.0,
        ..Default::default()
    };
    let tracker = CostTracker::new(config.clone(), tmp.path()).unwrap();
    let before: CostSummary = tracker.get_summary_for_task("task-1").unwrap();
    assert_eq!(before.request_count, 1);
    assert_eq!(before.total_tokens, 15);
    assert!((before.session_cost_usd - 0.001).abs() < 1e-9);

    tracker
        .record_usage_with_task_attribution(
            calculate_token_usage("test/model", 1_000, 500, 200, 3.0, 15.0, 0.5),
            Some("agent-a"),
            Some("task-1"),
        )
        .unwrap();
    let persisted = fs::read_to_string(&path).unwrap();
    assert!(
        persisted.starts_with(legacy),
        "old rows must not be repriced"
    );
    let records: Vec<CostRecord> = persisted
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].usage.cached_input_tokens, 0);
    assert!(records[0].usage.pricing_available);
    assert!(records[0].agent_alias.is_none());
    assert_eq!(records[1].agent_alias.as_deref(), Some("agent-a"));
    assert_eq!(records[1].task_id.as_deref(), Some("task-1"));
    assert_eq!(records[1].usage.cached_input_tokens, 200);
    assert!((records[1].usage.cost_usd - 0.01).abs() < 1e-9);
    let wire: serde_json::Value = serde_json::from_str(persisted.lines().nth(1).unwrap()).unwrap();
    assert_eq!(wire["goal_task_id"], "task-1");
    assert!(wire.get("task_id").is_none());

    assert!(matches!(
        tracker.check_budget(0.0).unwrap(),
        BudgetCheck::Allowed
    ));
    tracker.update_config(CostConfig {
        daily_limit_usd: 0.005,
        ..config.clone()
    });
    assert!(matches!(
        tracker.check_budget(0.0).unwrap(),
        BudgetCheck::Exceeded {
            period: UsagePeriod::Day,
            ..
        }
    ));
    drop(tracker);

    let reopened = CostTracker::new(config, tmp.path()).unwrap();
    let summary: CostSummary = reopened.get_summary_for_task("task-1").unwrap();
    assert_eq!(summary.request_count, 2);
    assert_eq!(summary.total_tokens, 1515);
    assert!((summary.session_cost_usd - 0.011).abs() < 1e-9);
    assert_eq!(reopened.get_summary().unwrap().request_count, 0);
    assert_eq!(fs::read_to_string(path).unwrap(), persisted);
}
