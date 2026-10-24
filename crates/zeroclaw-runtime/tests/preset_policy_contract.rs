use zeroclaw_policy::SecurityPolicy;
use zeroclaw_runtime::quickstart::presets::{risk_preset, runtime_preset};

#[test]
fn balanced_risk_allows_routine_commands_but_blocks_high_risk() {
    let preset = risk_preset("balanced").unwrap();
    let values = (preset.values)();
    let policy = SecurityPolicy {
        autonomy: values.level,
        allowed_commands: values.allowed_commands.clone(),
        block_high_risk_commands: values.block_high_risk_commands,
        require_approval_for_medium_risk: values.require_approval_for_medium_risk,
        ..SecurityPolicy::default()
    };
    for cmd in ["ls", "cat README.md", "git status"] {
        assert!(
            policy.is_command_allowed(cmd),
            "balanced must allow routine command `{cmd}` without an allowlist",
        );
    }
    // High-risk command passes the allowlist but is blocked outright at
    // execution: the `*` wildcard is not an explicit allowlist entry, so
    // block_high_risk_commands rejects it even when approved=true. This is
    // a hard block, not an approval prompt.
    assert!(policy.is_command_allowed("rm -rf node_modules"));
    let err_unapproved = policy
        .validate_command_execution("rm -rf node_modules", false)
        .expect_err("balanced must block a wildcard-matched high-risk command");
    let err_approved = policy
        .validate_command_execution("rm -rf node_modules", true)
        .expect_err("blocked even with approved=true: not an approval prompt");
    assert!(
        err_approved.contains("high-risk command is disallowed"),
        "must be the hard-block error, not the approval-required one: {err_approved}",
    );
    assert_eq!(
        err_unapproved, err_approved,
        "approval state must not change the outcome for a wildcard-matched high-risk command",
    );
}

#[test]
fn unbounded_runtime_does_not_block_all_actions() {
    let preset = runtime_preset("unbounded").unwrap();
    let values = (preset.values)();
    assert_ne!(
        values.max_actions_per_hour, 0,
        "unbounded must not use 0 — 0 means a hard zero action budget, not unlimited",
    );
    let policy = SecurityPolicy {
        max_actions_per_hour: values.max_actions_per_hour,
        ..SecurityPolicy::default()
    };
    assert!(
        policy.record_action(),
        "an unbounded-profile agent must be allowed to take actions",
    );
}

#[test]
fn yolo_risk_allows_shell_commands() {
    let preset = risk_preset("yolo").unwrap();
    let values = (preset.values)();
    let policy = SecurityPolicy {
        autonomy: values.level,
        allowed_commands: values.allowed_commands.clone(),
        block_high_risk_commands: values.block_high_risk_commands,
        ..SecurityPolicy::default()
    };
    for cmd in ["ls", "pwd", "cat README.md", "rm -rf node_modules"] {
        assert!(
            policy.is_command_allowed(cmd),
            "yolo profile must allow `{cmd}` — it grants full autonomy with no denylist",
        );
    }
}
