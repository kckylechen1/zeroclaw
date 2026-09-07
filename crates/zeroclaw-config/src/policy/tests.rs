#[cfg(test)]
use super::*;

fn default_policy() -> SecurityPolicy {
    SecurityPolicy::default()
}

// Platform-specific test paths: Unix uses `/…` paths, Windows uses
// `C:\…` paths so that `Path::is_absolute()` returns the correct
// value on each platform.

#[cfg(not(target_os = "windows"))]
fn tp_ws() -> PathBuf {
    PathBuf::from("/home/user/.zeroclaw/workspace")
}
#[cfg(target_os = "windows")]
fn tp_ws() -> PathBuf {
    PathBuf::from("C:\\Users\\user\\.zeroclaw\\workspace")
}

#[cfg(not(target_os = "windows"))]
fn tp_ws_shared() -> PathBuf {
    PathBuf::from("/home/user/.zeroclaw/shared")
}
#[cfg(target_os = "windows")]
fn tp_ws_shared() -> PathBuf {
    PathBuf::from("C:\\Users\\user\\.zeroclaw\\shared")
}

#[cfg(not(target_os = "windows"))]
fn tp_outside1() -> &'static str {
    "/home/user/other/file.txt"
}
#[cfg(target_os = "windows")]
fn tp_outside1() -> &'static str {
    "C:\\Users\\user\\other\\file.txt"
}

#[cfg(not(target_os = "windows"))]
fn tp_outside2() -> &'static str {
    "/tmp/file.txt"
}
#[cfg(target_os = "windows")]
fn tp_outside2() -> &'static str {
    "C:\\Users\\Public\\file.txt"
}

#[cfg(not(target_os = "windows"))]
fn tp_sys() -> &'static str {
    "/etc"
}
#[cfg(target_os = "windows")]
fn tp_sys() -> &'static str {
    "C:\\Windows\\System32"
}

#[cfg(not(target_os = "windows"))]
fn tp_sys_sub(sub: &str) -> String {
    format!("/{sub}")
}
#[cfg(target_os = "windows")]
fn tp_sys_sub(sub: &str) -> String {
    format!("C:\\Windows\\{}", sub.replace('/', "\\"))
}

#[cfg(not(target_os = "windows"))]
fn tp_proj() -> PathBuf {
    PathBuf::from("/projects")
}
#[cfg(target_os = "windows")]
fn tp_proj() -> PathBuf {
    PathBuf::from("C:\\projects")
}

#[cfg(not(target_os = "windows"))]
fn tp_data() -> PathBuf {
    PathBuf::from("/data")
}
#[cfg(target_os = "windows")]
fn tp_data() -> PathBuf {
    PathBuf::from("C:\\data")
}

#[cfg(not(target_os = "windows"))]
fn tp_rw() -> PathBuf {
    PathBuf::from("/rw-data")
}
#[cfg(target_os = "windows")]
fn tp_rw() -> PathBuf {
    PathBuf::from("C:\\rw-data")
}

#[cfg(not(target_os = "windows"))]
fn tp_ro() -> PathBuf {
    PathBuf::from("/ro-shared")
}
#[cfg(target_os = "windows")]
fn tp_ro() -> PathBuf {
    PathBuf::from("C:\\ro-shared")
}

#[test]
fn is_tool_allowed_none_is_unrestricted() {
    let p = SecurityPolicy {
        allowed_tools: None,
        excluded_tools: None,
        ..SecurityPolicy::default()
    };
    assert!(p.is_tool_allowed("shell"));
    assert!(p.is_tool_allowed("spawn_subagent"));
    assert!(p.is_tool_allowed("anything_else"));
}

#[test]
fn is_tool_allowed_some_empty_denies_all() {
    let p = SecurityPolicy {
        allowed_tools: Some(vec![]),
        ..SecurityPolicy::default()
    };
    assert!(!p.is_tool_allowed("shell"));
    assert!(!p.is_tool_allowed("spawn_subagent"));
}

#[test]
fn is_tool_allowed_allowlist_admits_only_listed() {
    let p = SecurityPolicy {
        allowed_tools: Some(vec!["shell".into(), "memory_recall".into()]),
        ..SecurityPolicy::default()
    };
    assert!(p.is_tool_allowed("shell"));
    assert!(p.is_tool_allowed("memory_recall"));
    assert!(!p.is_tool_allowed("spawn_subagent"));
    assert!(!p.is_tool_allowed("file_write"));
}

#[test]
fn is_tool_allowed_excluded_overrides_allowlist() {
    let p = SecurityPolicy {
        allowed_tools: Some(vec!["shell".into(), "spawn_subagent".into()]),
        excluded_tools: Some(vec!["spawn_subagent".into()]),
        ..SecurityPolicy::default()
    };
    assert!(p.is_tool_allowed("shell"));
    assert!(
        !p.is_tool_allowed("spawn_subagent"),
        "excluded_tools must subtract from allowlist"
    );
}

#[test]
fn is_tool_allowed_excluded_alone_subtracts_from_unrestricted() {
    let p = SecurityPolicy {
        allowed_tools: None,
        excluded_tools: Some(vec!["spawn_subagent".into()]),
        ..SecurityPolicy::default()
    };
    assert!(p.is_tool_allowed("shell"));
    assert!(!p.is_tool_allowed("spawn_subagent"));
}

#[test]
fn is_tool_excluded_reflects_denylist_independent_of_allowlist() {
    // No denylist → nothing excluded, regardless of allowlist.
    let none = SecurityPolicy {
        allowed_tools: Some(vec!["shell".into()]),
        excluded_tools: None,
        ..SecurityPolicy::default()
    };
    assert!(!none.is_tool_excluded("shell"));
    assert!(
        !none.is_tool_excluded("deploy__run"),
        "a tool omitted from the allowlist is not 'excluded' — the denylist is separate"
    );

    // Denylist subtracts by name, independent of the allowlist.
    let denied = SecurityPolicy {
        allowed_tools: None,
        excluded_tools: Some(vec!["deploy__status".into()]),
        ..SecurityPolicy::default()
    };
    assert!(denied.is_tool_excluded("deploy__status"));
    assert!(!denied.is_tool_excluded("deploy__run"));
}

#[test]
fn from_profiles_propagates_every_risk_profile_field() {
    use crate::schema::RiskProfileConfig;
    use std::path::Path;

    let rp = RiskProfileConfig {
        level: AutonomyLevel::ReadOnly,
        workspace_only: true,
        allowed_commands: vec!["only_this".into()],
        forbidden_paths: vec!["/secret".into()],
        require_approval_for_medium_risk: false,
        block_high_risk_commands: false,
        shell_env_passthrough: vec!["EDITOR".into(), "PAGER".into()],
        auto_approve: vec!["memory_recall".into()],
        always_ask: vec!["shell".into()],
        allowed_roots: vec!["/tmp/extra".into()],
        approval_route: None,
        allowed_tools: Some(vec!["shell".into(), "memory_recall".into()]),
        excluded_tools: vec!["spawn_subagent".into()],
        // Deliberately the non-default variant: propagation of a field
        // whose value equals the default is indistinguishable from the
        // field being dropped on the floor.
        mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit,
        sandbox_enabled: Some(true),
        sandbox_backend: Some("firejail".into()),
        firejail_args: vec!["--net=none".into()],
    };

    let policy = SecurityPolicy::from_profiles(&rp, None, Path::new("/ws"));

    assert_eq!(policy.autonomy, AutonomyLevel::ReadOnly, "level → autonomy");
    assert!(policy.workspace_only, "workspace_only");
    assert_eq!(policy.allowed_commands, vec!["only_this".to_string()]);
    assert_eq!(policy.forbidden_paths, vec!["/secret".to_string()]);
    assert!(!policy.require_approval_for_medium_risk);
    assert!(!policy.block_high_risk_commands);
    assert_eq!(
        policy.shell_env_passthrough,
        vec!["EDITOR".to_string(), "PAGER".to_string()]
    );
    assert_eq!(
        policy.auto_approve,
        vec!["memory_recall".to_string()],
        "auto_approve must reach the policy"
    );
    assert_eq!(
        policy.always_ask,
        vec!["shell".to_string()],
        "always_ask must reach the policy"
    );
    assert!(
        policy.allowed_roots.iter().any(|p| p.ends_with("extra")),
        "allowed_roots expansion must reach the policy"
    );
    assert_eq!(
        policy.allowed_tools.as_deref(),
        Some(&["shell".to_string(), "memory_recall".to_string()][..]),
        "allowed_tools must reach the policy"
    );
    assert_eq!(
        policy.excluded_tools.as_deref(),
        Some(&["spawn_subagent".to_string()][..]),
        "excluded_tools must reach the policy"
    );
    assert_eq!(
        policy.mcp_discovered_tool_policy,
        crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit,
        "mcp_discovered_tool_policy must reach the policy"
    );
    assert_eq!(policy.sandbox_enabled, Some(true), "sandbox_enabled");
    assert_eq!(
        policy.sandbox_backend.as_deref(),
        Some("firejail"),
        "sandbox_backend"
    );
    assert_eq!(
        policy.firejail_args,
        vec!["--net=none".to_string()],
        "firejail_args"
    );
}

#[test]
fn from_profiles_full_autonomy_drops_workspace_only() {
    use crate::schema::RiskProfileConfig;
    use std::path::Path;

    let rp = RiskProfileConfig {
        level: AutonomyLevel::Full,
        workspace_only: true,
        ..RiskProfileConfig::default()
    };

    let policy = SecurityPolicy::from_profiles(&rp, None, Path::new("/ws"));
    assert!(
        !policy.workspace_only,
        "Full autonomy must drop workspace_only even when the profile sets it true"
    );
}

#[test]
fn from_profiles_absent_allowed_tools_means_unrestricted() {
    use crate::schema::RiskProfileConfig;
    use std::path::Path;

    let risk = RiskProfileConfig {
        allowed_tools: None,
        ..RiskProfileConfig::default()
    };

    let policy = SecurityPolicy::from_profiles(&risk, None, Path::new("/ws"));

    assert!(
        policy.allowed_tools.is_none(),
        "absent allowed_tools must remain unrestricted (None)"
    );
    assert!(
        policy.is_tool_allowed("filesystem__write_file"),
        "absent risk-profile allowed_tools is unrestricted"
    );
}

/// G1: explicit `allowed_tools = []` in TOML must deny all tools.
#[test]
fn risk_profile_allowed_tools_empty_toml_denies_all_tools() {
    use crate::schema::Config;
    use std::path::Path;

    let toml = r#"
schema_version = 2

[risk_profiles.deny_all]
allowed_tools = []

[agents.default]
risk_profile = "deny_all"
"#;
    let config: Config = toml::from_str(toml).expect("deny-all risk profile TOML");
    let risk = config
        .risk_profiles
        .get("deny_all")
        .expect("deny_all risk profile");
    let policy = SecurityPolicy::from_profiles(risk, None, Path::new("/ws"));

    assert_eq!(
        policy.allowed_tools.as_deref(),
        Some(&[][..]),
        "explicit empty allowed_tools must map to Some(empty) deny-all"
    );
    assert!(
        !policy.is_tool_allowed("arbitrary_tool_name"),
        "explicit empty allowed_tools must deny an arbitrary tool"
    );
}

/// G2: omitted `allowed_tools` key stays unrestricted (`None`).
#[test]
fn risk_profile_allowed_tools_absent_toml_is_unrestricted() {
    use crate::schema::Config;
    use std::path::Path;

    let toml = r#"
schema_version = 2

[risk_profiles.unrestricted]

[agents.default]
risk_profile = "unrestricted"
"#;
    let config: Config = toml::from_str(toml).expect("unrestricted risk profile TOML");
    let risk = config
        .risk_profiles
        .get("unrestricted")
        .expect("unrestricted risk profile");
    let policy = SecurityPolicy::from_profiles(risk, None, Path::new("/ws"));

    assert!(
        policy.allowed_tools.is_none(),
        "absent allowed_tools must remain unrestricted (None)"
    );
    assert!(
        policy.is_tool_allowed("shell"),
        "absent allowed_tools must admit tools"
    );
}

/// G3: non-empty `allowed_tools` list is preserved.
#[test]
fn risk_profile_allowed_tools_nonempty_toml_preserves_list() {
    use crate::schema::Config;
    use std::path::Path;

    let toml = r#"
schema_version = 2

[risk_profiles.shell_only]
allowed_tools = ["shell"]

[agents.default]
risk_profile = "shell_only"
"#;
    let config: Config = toml::from_str(toml).expect("shell_only risk profile TOML");
    let risk = config
        .risk_profiles
        .get("shell_only")
        .expect("shell_only risk profile");
    let policy = SecurityPolicy::from_profiles(risk, None, Path::new("/ws"));

    assert_eq!(
        policy.allowed_tools.as_deref(),
        Some(&["shell".to_string()][..]),
        "non-empty allowed_tools must reach SecurityPolicy unchanged"
    );
    assert!(policy.is_tool_allowed("shell"));
    assert!(!policy.is_tool_allowed("memory_recall"));
}

#[test]
fn from_profiles_with_runtime_profile_propagates_budget_caps() {
    use crate::schema::RuntimeProfileConfig;
    use std::path::Path;

    let risk = crate::schema::RiskProfileConfig {
        level: AutonomyLevel::Supervised,
        ..crate::schema::RiskProfileConfig::default()
    };
    let runtime = RuntimeProfileConfig {
        max_actions_per_hour: 99,
        max_cost_per_day_cents: 1234,
        shell_timeout_secs: 300,
        ..RuntimeProfileConfig::default()
    };

    let policy = SecurityPolicy::from_profiles(&risk, Some(&runtime), Path::new("/ws"));

    assert_eq!(policy.max_actions_per_hour, 99);
    assert_eq!(policy.max_cost_per_day_cents, 1234);
    assert_eq!(policy.shell_timeout_secs, 300);
}

#[test]
fn from_profiles_without_runtime_profile_uses_defaults() {
    use std::path::Path;

    let risk = crate::schema::RiskProfileConfig {
        level: AutonomyLevel::Supervised,
        ..crate::schema::RiskProfileConfig::default()
    };

    let policy = SecurityPolicy::from_profiles(&risk, None, Path::new("/ws"));

    assert_eq!(policy.max_actions_per_hour, 20);
    assert_eq!(policy.max_cost_per_day_cents, 500);
    assert_eq!(policy.shell_timeout_secs, 60);
}

fn unix_forbidden_path_policy() -> SecurityPolicy {
    SecurityPolicy {
        workspace_dir: PathBuf::from("/workspace"),
        forbidden_paths: vec!["/dev".into(), "/etc".into()],
        ..SecurityPolicy::default()
    }
}

fn readonly_policy() -> SecurityPolicy {
    SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    }
}

fn full_policy() -> SecurityPolicy {
    SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        ..SecurityPolicy::default()
    }
}

// ── AutonomyLevel ────────────────────────────────────────

#[test]
fn autonomy_default_is_supervised() {
    assert_eq!(AutonomyLevel::default(), AutonomyLevel::Supervised);
}

#[test]
fn autonomy_serde_roundtrip() {
    let json = serde_json::to_string(&AutonomyLevel::Full).unwrap();
    assert_eq!(json, "\"full\"");
    let parsed: AutonomyLevel = serde_json::from_str("\"readonly\"").unwrap();
    assert_eq!(parsed, AutonomyLevel::ReadOnly);
    let parsed2: AutonomyLevel = serde_json::from_str("\"supervised\"").unwrap();
    assert_eq!(parsed2, AutonomyLevel::Supervised);
}

#[test]
fn can_act_readonly_false() {
    assert!(!readonly_policy().can_act());
}

#[test]
fn can_act_supervised_true() {
    assert!(default_policy().can_act());
}

#[test]
fn can_act_full_true() {
    assert!(full_policy().can_act());
}

#[test]
fn enforce_tool_operation_read_allowed_in_readonly_mode() {
    let p = readonly_policy();
    assert!(
        p.enforce_tool_operation(ToolOperation::Read, "memory_recall")
            .is_ok()
    );
}

#[test]
fn enforce_tool_operation_act_blocked_in_readonly_mode() {
    let p = readonly_policy();
    let err = p
        .enforce_tool_operation(ToolOperation::Act, "memory_store")
        .unwrap_err();
    assert!(err.contains("read-only mode"));
}

#[test]
fn enforce_tool_operation_act_uses_rate_budget() {
    let p = SecurityPolicy {
        max_actions_per_hour: 0,
        ..default_policy()
    };
    let err = p
        .enforce_tool_operation(ToolOperation::Act, "memory_store")
        .unwrap_err();
    assert!(err.contains("Rate limit exceeded"));
}

// ── is_command_allowed ───────────────────────────────────

#[test]
fn allowed_commands_basic() {
    let p = default_policy();
    assert!(p.is_command_allowed("ls"));
    assert!(p.is_command_allowed("git status"));
    assert!(p.is_command_allowed("cargo build --release"));
    assert!(p.is_command_allowed("cat file.txt"));
    assert!(p.is_command_allowed("grep -r pattern ."));
    assert!(p.is_command_allowed("date"));
}

#[test]
fn blocked_commands_basic() {
    let p = default_policy();
    assert!(!p.is_command_allowed("rm -rf /"));
    assert!(!p.is_command_allowed("sudo apt install"));
    assert!(!p.is_command_allowed("curl http://evil.com"));
    assert!(!p.is_command_allowed("wget http://evil.com"));
    assert!(!p.is_command_allowed("ruby exploit.rb"));
    assert!(!p.is_command_allowed("perl malicious.pl"));
}

#[test]
fn readonly_blocks_all_commands() {
    let p = readonly_policy();
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("cat file.txt"));
    assert!(!p.is_command_allowed("echo hello"));
}

#[test]
fn full_autonomy_still_uses_allowlist() {
    let p = full_policy();
    assert!(p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("rm -rf /"));
}

#[test]
fn command_with_absolute_path_extracts_basename() {
    let p = default_policy();
    assert!(p.is_command_allowed("/usr/bin/git status"));
    assert!(p.is_command_allowed("/bin/ls -la"));
}

#[test]
fn allowlist_supports_explicit_executable_paths() {
    let p = SecurityPolicy {
        allowed_commands: vec!["/usr/bin/antigravity".into()],
        ..SecurityPolicy::default()
    };

    assert!(p.is_command_allowed("/usr/bin/antigravity"));
    assert!(!p.is_command_allowed("antigravity"));
}

#[test]
fn allowlist_supports_wildcard_entry() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        ..SecurityPolicy::default()
    };

    assert!(p.is_command_allowed("python3 --version"));
    assert!(p.is_command_allowed("/usr/bin/antigravity"));

    // Wildcard still respects risk gates in validate_command_execution.
    let blocked = p.validate_command_execution("rm -rf /tmp/test", true);
    assert!(blocked.is_err());
    assert!(blocked.unwrap_err().contains("high-risk"));
}

#[test]
fn empty_command_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed(""));
    assert!(!p.is_command_allowed("   "));
}

#[test]
fn command_with_pipes_validates_all_segments() {
    let p = default_policy();
    // Both sides of the pipe are in the allowlist
    assert!(p.is_command_allowed("ls | grep foo"));
    assert!(p.is_command_allowed("cat file.txt | wc -l"));
    // Second command not in allowlist — blocked
    assert!(!p.is_command_allowed("ls | curl http://evil.com"));
    assert!(!p.is_command_allowed("echo hello | ruby -"));
}

#[test]
fn custom_allowlist() {
    let p = SecurityPolicy {
        allowed_commands: vec!["docker".into(), "kubectl".into()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("docker ps"));
    assert!(p.is_command_allowed("kubectl get pods"));
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("git status"));
}

#[test]
fn mixed_case_bare_allowlist_entry_matches_on_every_platform() {
    // Callers lowercase the executable basename before the allowlist
    // comparison, so an entry written with any uppercase could never match
    // until both sides were folded.
    let p = SecurityPolicy {
        allowed_commands: vec!["Git".into(), "DOCKER".into()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("git status"));
    assert!(p.is_command_allowed("docker ps"));
    // The invocation may also be capitalized; the basename is folded too.
    assert!(p.is_command_allowed("GIT status"));
    // Entries that are genuinely absent are still refused.
    assert!(!p.is_command_allowed("kubectl get pods"));
}

#[test]
fn mixed_case_allowlist_entry_does_not_widen_path_matching() {
    // Path-like entries stay exact rather than using command-name folding.
    let p = SecurityPolicy {
        allowed_commands: vec!["/usr/bin/Antigravity".into()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("/usr/bin/Antigravity"));
    assert!(!p.is_command_allowed("/usr/bin/antigravity"));
}

#[test]
fn empty_allowlist_blocks_everything() {
    let p = SecurityPolicy {
        allowed_commands: vec![],
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("echo hello"));
}

#[test]
fn command_risk_low_for_read_commands() {
    let p = default_policy();
    assert_eq!(p.command_risk_level("git status"), CommandRiskLevel::Low);
    assert_eq!(p.command_risk_level("ls -la"), CommandRiskLevel::Low);
}

#[test]
fn command_risk_medium_for_mutating_commands() {
    let p = SecurityPolicy {
        allowed_commands: vec!["git".into(), "touch".into()],
        ..SecurityPolicy::default()
    };
    assert_eq!(
        p.command_risk_level("git reset --hard HEAD~1"),
        CommandRiskLevel::Medium
    );
    assert_eq!(
        p.command_risk_level("touch file.txt"),
        CommandRiskLevel::Medium
    );
}

#[test]
fn command_risk_high_for_dangerous_commands() {
    let p = SecurityPolicy {
        allowed_commands: vec!["rm".into()],
        ..SecurityPolicy::default()
    };
    assert_eq!(
        p.command_risk_level("rm -rf /tmp/test"),
        CommandRiskLevel::High
    );
}

#[test]
fn validate_command_requires_approval_for_medium_risk() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        require_approval_for_medium_risk: true,
        allowed_commands: vec!["touch".into()],
        ..SecurityPolicy::default()
    };

    let denied = p.validate_command_execution("touch test.txt", false);
    assert!(denied.is_err());
    assert!(denied.unwrap_err().contains("requires explicit approval"),);

    let allowed = p.validate_command_execution("touch test.txt", true);
    assert_eq!(allowed.unwrap(), CommandRiskLevel::Medium);
}

#[test]
fn validate_command_blocks_high_risk_via_wildcard() {
    // Wildcard allows the command through is_command_allowed, but
    // block_high_risk_commands still rejects it because "*" does not
    // count as an explicit allowlist entry.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        allowed_commands: vec!["*".into()],
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("rm -rf /tmp/test", true);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("high-risk"));
}

#[test]
fn validate_command_allows_explicitly_listed_high_risk() {
    // When a high-risk command is explicitly in allowed_commands, the
    // block_high_risk_commands gate is bypassed — the operator has made
    // a deliberate decision to permit it.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        allowed_commands: vec!["curl".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("curl https://api.example.com/data", true);
    assert_eq!(result.unwrap(), CommandRiskLevel::High);
}

#[test]
fn validate_command_allows_wget_when_explicitly_listed() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        allowed_commands: vec!["wget".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("wget https://releases.example.com/v1.tar.gz", true);
    assert_eq!(result.unwrap(), CommandRiskLevel::High);
}

#[test]
fn validate_command_blocks_non_listed_high_risk_when_another_is_allowed() {
    // Allowing curl explicitly should not exempt wget.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        allowed_commands: vec!["curl".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("wget https://evil.com", true);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not allowed"));
}

#[test]
fn validate_command_explicit_rm_bypasses_high_risk_block() {
    // Operator explicitly listed "rm" — they accept the risk.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        allowed_commands: vec!["rm".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("rm -rf /tmp/test", true);
    assert_eq!(result.unwrap(), CommandRiskLevel::High);
}

#[test]
fn validate_command_high_risk_still_needs_approval_in_supervised() {
    // Even when explicitly allowed, supervised mode still requires
    // approval for high-risk commands (the approval gate is separate
    // from the block gate).
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        allowed_commands: vec!["curl".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };

    let denied = p.validate_command_execution("curl https://api.example.com", false);
    assert!(denied.is_err());
    assert!(denied.unwrap_err().contains("requires explicit approval"));

    let allowed = p.validate_command_execution("curl https://api.example.com", true);
    assert_eq!(allowed.unwrap(), CommandRiskLevel::High);
}

#[test]
fn validate_command_pipe_needs_all_segments_explicitly_allowed() {
    // When a pipeline contains a high-risk command, every segment
    // must be explicitly allowed for the exemption to apply.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        allowed_commands: vec!["curl".into(), "grep".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("curl https://api.example.com | grep data", true);
    assert_eq!(result.unwrap(), CommandRiskLevel::High);
}

#[test]
fn validate_command_full_mode_skips_medium_risk_approval_gate() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        require_approval_for_medium_risk: true,
        allowed_commands: vec!["touch".into()],
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("touch test.txt", false);
    assert_eq!(result.unwrap(), CommandRiskLevel::Medium);
}

#[test]
fn validate_command_rejects_background_chain_bypass() {
    let p = default_policy();
    let result = p.validate_command_execution("ls & python3 -c 'print(1)'", false);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not allowed"));
}

// ── is_path_allowed ─────────────────────────────────────

#[test]
fn relative_paths_allowed() {
    let p = default_policy();
    assert!(p.is_path_allowed("file.txt"));
    assert!(p.is_path_allowed("src/main.rs"));
    assert!(p.is_path_allowed("deep/nested/dir/file.txt"));
}

#[test]
fn path_traversal_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("../etc/passwd"));
    assert!(!p.is_path_allowed("../../root/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("foo/../../../etc/shadow"));
    assert!(!p.is_path_allowed(".."));
}

#[test]
fn absolute_paths_blocked_when_workspace_only() {
    let p = default_policy();
    assert!(!p.is_path_allowed(&tp_sys_sub("etc/passwd")));
    assert!(!p.is_path_allowed(&tp_sys_sub("root/.ssh/id_rsa")));
    assert!(!p.is_path_allowed(tp_outside2()));
}

#[test]
fn absolute_path_inside_workspace_allowed_when_workspace_only() {
    let ws = tp_ws();
    let p = SecurityPolicy {
        workspace_dir: ws.clone(),
        workspace_only: true,
        ..SecurityPolicy::default()
    };
    assert!(p.is_path_allowed(&format!("{}/images/example.png", ws.display())));
    assert!(p.is_path_allowed(&format!("{}/file.txt", ws.display())));
    assert!(!p.is_path_allowed(tp_outside1()));
    assert!(!p.is_path_allowed(tp_outside2()));
}

#[test]
fn absolute_path_in_allowed_root_permitted_when_workspace_only() {
    let ws = tp_ws();
    let shared = tp_ws_shared();
    let p = SecurityPolicy {
        workspace_dir: ws.clone(),
        workspace_only: true,
        allowed_roots: vec![shared.clone()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_path_allowed(&format!("{}/data.txt", shared.display())));
    assert!(p.is_path_allowed(&format!("{}/file.txt", ws.display())));
    assert!(!p.is_path_allowed(tp_outside1()));
}

#[test]
fn absolute_paths_allowed_when_not_workspace_only() {
    let p = SecurityPolicy {
        workspace_only: false,
        forbidden_paths: vec![],
        ..SecurityPolicy::default()
    };
    assert!(p.is_path_allowed("/tmp/file.txt"));
}

#[test]
fn forbidden_paths_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed(&tp_sys_sub("etc/passwd")));
    assert!(!p.is_path_allowed(&tp_sys_sub("root/.bashrc")));
    assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("~/.gnupg/pubring.kbx"));
}

#[test]
fn empty_path_allowed() {
    let p = default_policy();
    assert!(p.is_path_allowed(""));
}

#[test]
fn dotfile_in_workspace_allowed() {
    let p = default_policy();
    assert!(p.is_path_allowed(".gitignore"));
    assert!(p.is_path_allowed(".env"));
}

// ── from_config ─────────────────────────────────────────

#[test]
fn from_config_maps_all_fields() {
    let risk = crate::schema::RiskProfileConfig {
        level: AutonomyLevel::Full,
        workspace_only: false,
        allowed_commands: vec!["docker".into()],
        forbidden_paths: vec!["/secret".into()],
        require_approval_for_medium_risk: false,
        block_high_risk_commands: false,
        shell_env_passthrough: vec!["DATABASE_URL".into()],
        ..crate::schema::RiskProfileConfig::default()
    };
    let runtime = crate::schema::RuntimeProfileConfig {
        max_actions_per_hour: 100,
        max_cost_per_day_cents: 1000,
        ..crate::schema::RuntimeProfileConfig::default()
    };
    let workspace = PathBuf::from("/tmp/test-workspace");
    let policy = SecurityPolicy::from_profiles(&risk, Some(&runtime), &workspace);

    assert_eq!(policy.autonomy, AutonomyLevel::Full);
    assert!(!policy.workspace_only);
    assert_eq!(policy.allowed_commands, vec!["docker"]);
    assert_eq!(policy.forbidden_paths, vec!["/secret"]);
    assert_eq!(policy.max_actions_per_hour, 100);
    assert_eq!(policy.max_cost_per_day_cents, 1000);
    assert!(!policy.require_approval_for_medium_risk);
    assert!(!policy.block_high_risk_commands);
    assert_eq!(policy.shell_env_passthrough, vec!["DATABASE_URL"]);
    assert_eq!(policy.workspace_dir, PathBuf::from("/tmp/test-workspace"));
}

#[test]
fn from_config_full_autonomy_overrides_workspace_only() {
    //: Full autonomy should disable workspace_only even if the
    // config default keeps it true.
    let autonomy_config = crate::schema::RiskProfileConfig {
        level: AutonomyLevel::Full,
        ..crate::schema::RiskProfileConfig::default()
    };
    let workspace = PathBuf::from("/tmp/test-workspace");
    let policy = SecurityPolicy::from_risk_profile(&autonomy_config, &workspace);

    assert_eq!(policy.autonomy, AutonomyLevel::Full);
    assert!(
        !policy.workspace_only,
        "Full autonomy must override workspace_only to false"
    );
}

#[test]
fn from_config_supervised_preserves_workspace_only() {
    let autonomy_config = crate::schema::RiskProfileConfig {
        level: AutonomyLevel::Supervised,
        ..crate::schema::RiskProfileConfig::default()
    };
    let workspace = PathBuf::from("/tmp/test-workspace");
    let policy = SecurityPolicy::from_risk_profile(&autonomy_config, &workspace);

    assert!(
        policy.workspace_only,
        "Supervised autonomy must preserve workspace_only default (true)"
    );
}

#[test]
fn from_config_normalizes_allowed_roots() {
    let autonomy_config = crate::schema::RiskProfileConfig {
        allowed_roots: vec!["~/Desktop".into(), "shared-data".into()],
        ..crate::schema::RiskProfileConfig::default()
    };
    let workspace = tp_ws();
    let policy = SecurityPolicy::from_risk_profile(&autonomy_config, &workspace);

    let expected_home_root = if let Some(home) = home_dir() {
        home.join("Desktop")
    } else {
        PathBuf::from("~/Desktop")
    };

    assert_eq!(policy.allowed_roots[0], expected_home_root);
    assert_eq!(policy.allowed_roots[1], workspace.join("shared-data"));
}

#[test]
fn resolved_path_violation_message_includes_allowed_roots_guidance() {
    let p = default_policy();
    let msg = p.resolved_path_violation_message(Path::new("/tmp/outside.txt"));
    assert!(msg.contains("escapes workspace"));
    assert!(msg.contains("allowed_roots"));
}

// ── Default policy ──────────────────────────────────────

#[test]
fn default_policy_has_sane_values() {
    let p = SecurityPolicy::default();
    assert_eq!(p.autonomy, AutonomyLevel::Supervised);
    assert!(p.workspace_only);
    assert!(!p.allowed_commands.is_empty());
    assert!(!p.forbidden_paths.is_empty());
    assert!(p.max_actions_per_hour > 0);
    assert!(p.max_cost_per_day_cents > 0);
    assert!(p.require_approval_for_medium_risk);
    assert!(p.block_high_risk_commands);
    assert!(p.shell_env_passthrough.is_empty());
}

// ── ActionTracker / rate limiting ───────────────────────

#[test]
fn action_tracker_starts_at_zero() {
    let tracker = ActionTracker::new();
    assert_eq!(tracker.count(), 0);
}

#[test]
fn action_tracker_records_actions() {
    let tracker = ActionTracker::new();
    assert_eq!(tracker.record(), 1);
    assert_eq!(tracker.record(), 2);
    assert_eq!(tracker.record(), 3);
    assert_eq!(tracker.count(), 3);
}

#[test]
fn action_tracker_retains_actions_when_cutoff_is_unavailable() {
    let mut actions = vec![Instant::now(), Instant::now()];

    retain_actions_after(&mut actions, None);

    assert_eq!(actions.len(), 2);
}

#[test]
fn record_action_allows_within_limit() {
    let p = SecurityPolicy {
        max_actions_per_hour: 5,
        ..SecurityPolicy::default()
    };
    for _ in 0..5 {
        assert!(p.record_action(), "should allow actions within limit");
    }
}

#[test]
fn record_action_blocks_over_limit() {
    let p = SecurityPolicy {
        max_actions_per_hour: 3,
        ..SecurityPolicy::default()
    };
    assert!(p.record_action()); // 1
    assert!(p.record_action()); // 2
    assert!(p.record_action()); // 3
    assert!(!p.record_action()); // 4 — over limit
}

#[test]
fn is_rate_limited_reflects_count() {
    let p = SecurityPolicy {
        max_actions_per_hour: 2,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_rate_limited());
    p.record_action();
    assert!(!p.is_rate_limited());
    p.record_action();
    assert!(p.is_rate_limited());
}

#[test]
fn action_tracker_clone_is_independent() {
    let tracker = ActionTracker::new();
    tracker.record();
    tracker.record();
    let cloned = tracker.clone();
    assert_eq!(cloned.count(), 2);
    tracker.record();
    assert_eq!(tracker.count(), 3);
    assert_eq!(cloned.count(), 2); // clone is independent
}

// ── Edge cases: command injection ────────────────────────

#[test]
fn command_injection_semicolon_blocked() {
    let p = default_policy();
    // First word is "ls;" (with semicolon) — doesn't match "ls" in allowlist.
    // This is a safe default: chained commands are blocked.
    assert!(!p.is_command_allowed("ls; rm -rf /"));
}

#[test]
fn command_injection_semicolon_no_space() {
    let p = default_policy();
    assert!(!p.is_command_allowed("ls;rm -rf /"));
}

#[test]
fn quoted_semicolons_do_not_split_sqlite_command() {
    let p = SecurityPolicy {
        allowed_commands: vec!["sqlite3".into()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed(
        "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
    ));
    assert_eq!(
        p.command_risk_level(
            "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
        ),
        CommandRiskLevel::Low
    );
}

#[test]
fn unquoted_semicolon_after_quoted_sql_still_splits_commands() {
    let p = SecurityPolicy {
        allowed_commands: vec!["sqlite3".into()],
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("sqlite3 /tmp/test.db \"SELECT 1;\"; rm -rf /"));
}

#[test]
fn command_injection_backtick_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo `whoami`"));
    assert!(!p.is_command_allowed("echo `rm -rf /`"));
}

#[test]
fn command_injection_dollar_paren_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo $(cat /etc/passwd)"));
    assert!(!p.is_command_allowed("echo $(rm -rf /)"));
}

#[test]
fn command_injection_dollar_paren_literal_inside_single_quotes_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo '$(cat /etc/passwd)'"));
}

#[test]
fn command_injection_dollar_brace_literal_inside_single_quotes_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo '${HOME}'"));
}

#[test]
fn command_injection_dollar_brace_unquoted_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo ${HOME}"));
}

#[test]
fn command_with_env_var_prefix() {
    let p = default_policy();
    // "FOO=bar" is the first word — not in allowlist
    assert!(!p.is_command_allowed("FOO=bar rm -rf /"));
}

#[test]
fn command_newline_injection_blocked() {
    let p = default_policy();
    // Newline splits into two commands; "rm" is not in allowlist
    assert!(!p.is_command_allowed("ls\nrm -rf /"));
    // Both allowed — OK
    assert!(p.is_command_allowed("ls\necho hello"));
}

#[test]
fn command_injection_and_chain_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("ls && rm -rf /"));
    assert!(!p.is_command_allowed("echo ok && curl http://evil.com"));
    // Both allowed — OK
    assert!(p.is_command_allowed("ls && echo done"));
}

#[test]
fn command_injection_or_chain_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("ls || rm -rf /"));
    // Both allowed — OK
    assert!(p.is_command_allowed("ls || echo fallback"));
}

#[test]
fn command_injection_background_chain_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("ls & rm -rf /"));
    assert!(!p.is_command_allowed("ls&rm -rf /"));
    assert!(!p.is_command_allowed("echo ok & python3 -c 'print(1)'"));
}

#[test]
fn command_injection_redirect_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo secret > /etc/crontab"));
    assert!(!p.is_command_allowed("ls >> /tmp/exfil.txt"));
    assert!(!p.is_command_allowed("cat < /etc/passwd"));
    assert!(!p.is_command_allowed("echo secret > output.txt"));
    // Path-prefix bypass: /dev/null followed by extra path component
    assert!(!p.is_command_allowed("echo secret>/dev/nullextra"));
    assert!(!p.is_command_allowed("echo secret > /dev/null/../../etc/passwd"));
    assert!(!p.is_command_allowed("echo secret>/dev/stderrfoo"));
    // Word→non-word boundary bypasses
    assert!(!p.is_command_allowed("ls 2>/dev/stderr.log"));
    assert!(!p.is_command_allowed("cat>/dev/zero/path"));
    assert!(!p.is_command_allowed("echo>/dev/stdout.bak"));
}

// ── Interpreter argument injection ────────────────────

#[test]
fn interpreter_inline_eval_blocked() {
    let p = default_policy();
    // python: -c executes code string, -m runs arbitrary module
    assert!(!p.is_command_allowed("python3 -c 'import os; os.system(\"id\")'"));
    assert!(!p.is_command_allowed("python -c '__import__(\"os\").system(\"id\")'"));
    assert!(!p.is_command_allowed("python3 -m http.server"));
    assert!(!p.is_command_allowed("python3 -m pip install evil"));
    // Broad -m block: these are intentional collateral
    assert!(!p.is_command_allowed("python3 -m pytest"));
    assert!(!p.is_command_allowed("python3 -m mypy src/"));
    assert!(!p.is_command_allowed("python3 -m venv .venv"));
    // Glued form: -mhttp.server is one token
    assert!(!p.is_command_allowed("python3 -mhttp.server"));
    // node: -e/--eval evaluates JS, -p/--print evaluates and prints
    assert!(!p.is_command_allowed("node -e 'require(\"child_process\").execSync(\"id\")'"));
    assert!(!p.is_command_allowed("node --eval 'process.exit(1)'"));
    assert!(!p.is_command_allowed("node --eval=process.exit(1)"));
    assert!(!p.is_command_allowed("node -p '1+1'"));
    assert!(!p.is_command_allowed("node --print 'process.env'"));
    assert!(!p.is_command_allowed("node --print=process.env"));
    // Glued form bypass: -c'code' is one whitespace token
    assert!(!p.is_command_allowed("python3 -c'import os'"));
    assert!(!p.is_command_allowed("node -e'process.exit()'"));
    // Flag with other args before it
    assert!(!p.is_command_allowed("python3 -W ignore -c 'import os'"));
}

#[test]
fn package_manager_install_blocked() {
    let p = default_policy();
    // pip: install/download fetch external packages and run setup.py
    assert!(!p.is_command_allowed("pip install evil-package"));
    assert!(!p.is_command_allowed("pip3 install evil-package"));
    assert!(!p.is_command_allowed("pip download evil-package"));
    // npm: exec fetches remote, install runs lifecycle scripts
    assert!(!p.is_command_allowed("npm exec -- malicious-pkg"));
    assert!(!p.is_command_allowed("npm install malicious-pkg"));
    assert!(!p.is_command_allowed("npm i malicious-pkg"));
    assert!(!p.is_command_allowed("npm add malicious-pkg"));
    assert!(!p.is_command_allowed("npm ci"));
    // cargo: install fetches+builds external crate (build.rs runs arbitrary code)
    assert!(!p.is_command_allowed("cargo install malicious-crate"));
}

#[test]
fn safe_interpreter_usage_allowed() {
    let p = default_policy();
    // Running local files is safe — user trusts their workspace
    assert!(p.is_command_allowed("python3 script.py"));
    assert!(p.is_command_allowed("node app.js"));
    // Read-only / local workspace operations
    assert!(p.is_command_allowed("pip list"));
    assert!(p.is_command_allowed("pip freeze"));
    assert!(p.is_command_allowed("pip show requests"));
    assert!(p.is_command_allowed("npm test"));
    assert!(p.is_command_allowed("npm list"));
    assert!(p.is_command_allowed("cargo build"));
    assert!(p.is_command_allowed("cargo test"));
    assert!(p.is_command_allowed("cargo run"));
}

#[test]
fn safe_redirect_to_dev_null_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo secret > /dev/null"));
    assert!(p.is_command_allowed("ls 2> /dev/null"));
    assert!(p.is_command_allowed("find . 2>&1 > /dev/null"));
    assert!(p.is_command_allowed("cat</dev/null"));
}

#[test]
fn safe_redirect_to_dev_stdout_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo hello > /dev/stdout"));
    assert!(p.is_command_allowed("cat /dev/zero > /dev/stdout"));
}

#[test]
fn safe_redirect_to_dev_stderr_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo error > /dev/stderr"));
    assert!(p.is_command_allowed("ls 1> /dev/stderr"));
}

#[test]
fn safe_redirect_to_dev_zero_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("cat /dev/zero > /dev/null"));
}

#[test]
fn safe_file_descriptor_redirect_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("find . 2>&1"));
    assert!(p.is_command_allowed("echo hello 1>&2"));
    assert!(p.is_command_allowed("ls 2>&1 > /dev/null"));
    // Bare fd redirects (implicit fd number)
    assert!(p.is_command_allowed("echo error >&2"));
    assert!(p.is_command_allowed("cat <&0"));
    assert!(p.is_command_allowed("echo >&-"));
    assert!(p.is_command_allowed("echo 3>&-"));
}

#[test]
fn heredoc_and_herestring_allowed() {
    let p = default_policy();
    assert!(p.is_command_allowed("cat << 'EOF'"));
    assert!(p.is_command_allowed("cat <<EOF"));
    assert!(p.is_command_allowed("cat <<< 'hello'"));
    // Input redirects from files still blocked
    assert!(!p.is_command_allowed("cat < /etc/passwd"));
    // Output redirects to files still blocked
    assert!(!p.is_command_allowed("echo secret > output.txt"));
}

#[test]
fn multiline_heredoc_allowed() {
    let p = default_policy();
    // Multiline heredoc body must not be split into separate segments that
    // fail the allowlist check on the body lines.
    assert!(p.is_command_allowed("cat <<EOF\nhello world\nEOF"));
    assert!(p.is_command_allowed("cat <<'EOF'\nhello world\nEOF"));
    assert!(p.is_command_allowed("cat << EOF\nhello world\nEOF"));
    // Quoted delimiter variant
    assert!(p.is_command_allowed("cat <<\"EOF\"\nhello world\nEOF"));
    // Heredoc followed by an allowed command is still two valid segments
    assert!(p.is_command_allowed("cat <<EOF\nhello\nEOF\necho done"));
    // Heredoc followed by a disallowed command must be blocked
    assert!(!p.is_command_allowed("cat <<EOF\nhello\nEOF\nrm -rf /"));
    // Unterminated heredoc — entire input stays as one segment (safe: cat is allowed).
    assert!(p.is_command_allowed("cat <<EOF\nhello world"));
}

#[test]
fn redirect_helper_unit_tests() {
    assert!(!contains_unquoted_input_redirect("cat << 'EOF'"));
    assert!(!contains_unquoted_input_redirect("cat <<< 'hello'"));
    assert!(contains_unquoted_input_redirect("cat < /etc/passwd"));
    assert!(!contains_unquoted_input_redirect("echo 'a<b'"));
    assert!(!contains_unquoted_input_redirect("cat</dev/null"));
    // Input redirect word→non-word bypass (same fix as output redirects)
    assert!(contains_unquoted_input_redirect("cat</dev/null.secret"));
    assert!(contains_unquoted_input_redirect(
        "cat </dev/zero/etc/passwd"
    ));
    assert!(!contains_unsafe_output_redirect("cmd 2>/dev/null"));
    assert!(!contains_unsafe_output_redirect("cmd >/dev/null"));
    assert!(!contains_unsafe_output_redirect("cmd 1>/dev/null"));
    assert!(!contains_unsafe_output_redirect("cmd 2>&1"));
    assert!(!contains_unsafe_output_redirect("cmd 1>&2"));
    assert!(!contains_unsafe_output_redirect("echo > /dev/stdout"));
    assert!(!contains_unsafe_output_redirect("echo > /dev/stderr"));
    assert!(!contains_unsafe_output_redirect("echo > /dev/zero"));
    assert!(contains_unsafe_output_redirect("echo hi > file.txt"));
    assert!(!contains_unsafe_output_redirect("echo 'a>b'"));
    // Word→non-word boundary bypasses: dot, slash, or other non-operator chars
    // after a safe device name must NOT strip the redirect
    assert!(contains_unsafe_output_redirect("ls 2>/dev/stderr.log"));
    assert!(contains_unsafe_output_redirect("cat>/dev/zero/path"));
    assert!(contains_unsafe_output_redirect("echo>/dev/stdout.bak"));
}

#[test]
fn quoted_ampersand_and_redirect_literals_are_not_treated_as_operators() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo \"A&B\""));
    assert!(p.is_command_allowed("echo \"A>B\""));
    assert!(p.is_command_allowed("echo \"A<B\""));
}

#[test]
fn git_dash_c_uppercase_is_allowed() {
    // git -C (change directory) must not be
    // conflated with git -c (set config override) after arg lowercasing.
    let p = default_policy();
    assert!(
        p.is_command_allowed("git -C /home/user/repo status --short"),
        "git -C is benign and should be allowed"
    );
    assert!(
        p.is_command_allowed("git -C /home/user/repo log --oneline -1"),
        "git -C with log should be allowed"
    );
    // git -c (lowercase) is still blocked — config override injection
    assert!(
        !p.is_command_allowed("git -c core.editor=\"rm -rf /\" commit"),
        "git -c must remain blocked"
    );
}

#[test]
fn command_argument_injection_blocked() {
    let p = default_policy();
    // find -exec is a common bypass
    assert!(!p.is_command_allowed("find . -exec rm -rf {} +"));
    assert!(!p.is_command_allowed("find / -ok cat {} \\;"));
    // git config/alias can execute commands
    assert!(!p.is_command_allowed("git config core.editor \"rm -rf /\""));
    assert!(!p.is_command_allowed("git alias.st status"));
    assert!(!p.is_command_allowed("git -c core.editor=calc.exe commit"));
    // Legitimate commands should still work
    assert!(p.is_command_allowed("find . -name '*.txt'"));
    assert!(p.is_command_allowed("git status"));
    assert!(p.is_command_allowed("git add ."));
}

#[test]
fn command_injection_dollar_brace_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo ${IFS}cat${IFS}/etc/passwd"));
}

#[test]
fn command_injection_plain_dollar_var_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("cat $HOME/.ssh/id_rsa"));
    assert!(!p.is_command_allowed("cat $SECRET_FILE"));
}

#[test]
fn command_injection_tee_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo secret | tee /etc/crontab"));
    assert!(!p.is_command_allowed("ls | /usr/bin/tee outfile"));
    assert!(!p.is_command_allowed("tee file.txt"));
}

#[test]
fn command_injection_process_substitution_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("cat <(echo pwned)"));
    assert!(!p.is_command_allowed("ls >(cat /etc/passwd)"));
}

#[test]
fn command_env_var_prefix_with_allowed_cmd() {
    let p = default_policy();
    // env assignment + allowed command — OK
    assert!(p.is_command_allowed("FOO=bar ls"));
    assert!(p.is_command_allowed("LANG=C grep pattern file"));
    // env assignment + disallowed command — blocked
    assert!(!p.is_command_allowed("FOO=bar rm -rf /"));
}

#[test]
fn forbidden_path_argument_detects_absolute_path() {
    let p = unix_forbidden_path_policy();
    assert_eq!(
        p.forbidden_path_argument("cat /etc/passwd"),
        Some("/etc/passwd".into())
    );
}

#[test]
fn forbidden_path_argument_detects_parent_dir_reference() {
    let p = default_policy();
    assert_eq!(
        p.forbidden_path_argument("cat ../secret.txt"),
        Some("../secret.txt".into())
    );
    assert_eq!(
        p.forbidden_path_argument("find .. -name '*.rs'"),
        Some("..".into())
    );
}

#[test]
fn forbidden_path_argument_allows_workspace_relative_paths() {
    let p = default_policy();
    assert_eq!(p.forbidden_path_argument("cat src/main.rs"), None);
    assert_eq!(p.forbidden_path_argument("grep -r todo ./src"), None);
}

#[test]
fn forbidden_path_argument_detects_option_assignment_paths() {
    let p = unix_forbidden_path_policy();
    assert_eq!(
        p.forbidden_path_argument("grep --file=/etc/passwd root ./src"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat --input=../secret.txt"),
        Some("../secret.txt".into())
    );
}

#[test]
fn forbidden_path_argument_allows_safe_option_assignment_paths() {
    let p = default_policy();
    assert_eq!(
        p.forbidden_path_argument("grep --file=./patterns.txt root ./src"),
        None
    );
}

#[test]
fn forbidden_path_argument_detects_short_option_attached_paths() {
    let p = unix_forbidden_path_policy();
    assert_eq!(
        p.forbidden_path_argument("grep -f/etc/passwd root ./src"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("git -C../outside status"),
        Some("../outside".into())
    );
}

#[test]
fn forbidden_path_argument_allows_safe_short_option_attached_paths() {
    let p = default_policy();
    assert_eq!(
        p.forbidden_path_argument("grep -f./patterns.txt root ./src"),
        None
    );
    assert_eq!(p.forbidden_path_argument("git -C./repo status"), None);
}

#[test]
fn forbidden_path_argument_detects_tilde_user_paths() {
    let p = default_policy();
    assert_eq!(
        p.forbidden_path_argument("cat ~root/.ssh/id_rsa"),
        Some("~root/.ssh/id_rsa".into())
    );
    // Bare `~user` with no path component is not a forbidden path argument:
    // narrowed to avoid false-positives on non-path `~`-prefixed tokens
    // (e.g. `~20`). A `~user/...` form with a slash still blocks (above).
    assert_eq!(p.forbidden_path_argument("ls ~nobody"), None);
}

#[test]
fn forbidden_path_argument_ignores_tilde_non_path_and_heredoc_body() {
    let p = unix_forbidden_path_policy();

    // Tilde-then-non-slash tokens are not home paths: `~20`, `~589`, `~foo`.
    assert_eq!(
        p.forbidden_path_argument("echo \"about ~20 percent\""),
        None
    );
    assert_eq!(
        p.forbidden_path_argument("python3 -c \"print('about ~589 lines')\""),
        None
    );
    assert_eq!(
        p.forbidden_path_argument("printf 'roughly ~foo here\\n'"),
        None
    );

    // Heredoc body content is stdin data, never an argv path argument.
    let heredoc =
        "cat <<'EOF' > ./out.txt\nthis line has ~20 percent and /etc/passwd mentioned\nEOF";
    assert_eq!(p.forbidden_path_argument(heredoc), None);

    // Security preserved: real forbidden home/absolute path arguments still block.
    assert_eq!(
        p.forbidden_path_argument("cat ~/.ssh/id_rsa"),
        Some("~/.ssh/id_rsa".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat ~root/.ssh/id_rsa"),
        Some("~root/.ssh/id_rsa".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat /etc/shadow"),
        Some("/etc/shadow".into())
    );
    // A forbidden path used as a real argument on the heredoc opener line
    // must still block, even when a heredoc body follows.
    assert_eq!(
        p.forbidden_path_argument("cat /etc/passwd <<'EOF'\nbody ~20\nEOF"),
        Some("/etc/passwd".into())
    );
}

#[test]
fn forbidden_path_argument_blocks_path_after_quoted_heredoc_like_text() {
    let p = unix_forbidden_path_policy();

    assert_eq!(
        p.forbidden_path_argument("printf \"<<EOF\nbody\nEOF\" /etc/shadow"),
        Some("/etc/shadow".into())
    );

    // Single-quoted variant of the same shape.
    assert_eq!(
        p.forbidden_path_argument("printf '<<EOF\nbody\nEOF' /etc/passwd"),
        Some("/etc/passwd".into())
    );
}

#[test]
fn forbidden_path_argument_detects_input_redirection_paths() {
    let p = unix_forbidden_path_policy();
    assert_eq!(
        p.forbidden_path_argument("cat </etc/passwd"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat</etc/passwd"),
        Some("/etc/passwd".into())
    );
}

#[test]
fn forbidden_path_argument_allows_safe_device_redirect_targets() {
    let p = unix_forbidden_path_policy();
    assert_eq!(p.forbidden_path_argument("ls missing 2>/dev/null"), None);
    assert_eq!(p.forbidden_path_argument("ls missing 2> /dev/null"), None);
    assert_eq!(p.forbidden_path_argument("echo hi >/dev/stdout"), None);
    assert_eq!(p.forbidden_path_argument("echo hi > /dev/stdout"), None);
    assert_eq!(p.forbidden_path_argument("echo err 1>/dev/stderr"), None);
    assert_eq!(p.forbidden_path_argument("echo err 1> /dev/stderr"), None);
    assert_eq!(p.forbidden_path_argument("cat </dev/zero"), None);
    assert_eq!(p.forbidden_path_argument("cat < /dev/zero"), None);
    #[cfg(not(target_os = "windows"))]
    assert_eq!(p.forbidden_path_argument("cat /dev/null"), None);
    assert_eq!(p.forbidden_path_argument("cat ./safe.txt>/dev/null"), None);
    assert_eq!(p.forbidden_path_argument("cat> /dev/null"), None);
    assert_eq!(p.forbidden_path_argument("cat ./safe.txt>&2"), None);
}

#[test]
fn forbidden_path_argument_blocks_unsafe_redirect_targets() {
    let p = unix_forbidden_path_policy();
    assert_eq!(
        p.forbidden_path_argument("echo hi >/etc/passwd"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("echo hi > /etc/passwd"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("echo hi >/dev/stderr.log"),
        Some("/dev/stderr.log".into())
    );
    assert_eq!(
        p.forbidden_path_argument("echo hi > /dev/stderr.log"),
        Some("/dev/stderr.log".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat </dev/zero/etc/passwd"),
        Some("/dev/zero/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("echo hi >/dev/null/../../etc/passwd"),
        Some("/dev/null/../../etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat</dev/null /etc/passwd"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat /etc/passwd>/dev/null"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat /etc/passwd> /dev/null"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("cat /etc/passwd>&2"),
        Some("/etc/passwd".into())
    );
    assert_eq!(
        p.forbidden_path_argument("grep --file=/etc/passwd>/dev/null root"),
        Some("/etc/passwd".into())
    );
}

// ── Edge cases: path traversal ──────────────────────────

#[test]
fn path_traversal_encoded_dots() {
    let p = default_policy();
    // Literal ".." in path — always blocked
    assert!(!p.is_path_allowed("foo/..%2f..%2fetc/passwd"));
}

#[test]
fn path_traversal_double_dot_in_filename() {
    let p = default_policy();
    // ".." in a filename (not a path component) is allowed
    assert!(p.is_path_allowed("my..file.txt"));
    // But actual traversal components are still blocked
    assert!(!p.is_path_allowed("../etc/passwd"));
    assert!(!p.is_path_allowed("foo/../etc/passwd"));
}

#[test]
fn path_with_null_byte_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("file\0.txt"));
}

#[test]
fn path_symlink_style_absolute() {
    let p = default_policy();
    assert!(!p.is_path_allowed(&tp_sys_sub("proc/self/root/etc/passwd")));
}

#[test]
fn path_home_tilde_ssh() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("~/.gnupg/secring.gpg"));
    assert!(!p.is_path_allowed("~root/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("~nobody"));
}

#[test]
fn path_var_run_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed(&tp_sys_sub("var/run/docker.sock")));
}

// ── Edge cases: rate limiter boundary ────────────────────

#[test]
fn rate_limit_exactly_at_boundary() {
    let p = SecurityPolicy {
        max_actions_per_hour: 1,
        ..SecurityPolicy::default()
    };
    assert!(p.record_action()); // 1 — exactly at limit
    assert!(!p.record_action()); // 2 — over
    assert!(!p.record_action()); // 3 — still over
}

#[test]
fn rate_limit_zero_blocks_everything() {
    let p = SecurityPolicy {
        max_actions_per_hour: 0,
        ..SecurityPolicy::default()
    };
    assert!(!p.record_action());
}

#[test]
fn rate_limit_high_allows_many() {
    let p = SecurityPolicy {
        max_actions_per_hour: 10000,
        ..SecurityPolicy::default()
    };
    for _ in 0..100 {
        assert!(p.record_action());
    }
}

// ── Edge cases: autonomy + command combos ────────────────

#[test]
fn readonly_blocks_even_safe_commands() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        allowed_commands: vec!["ls".into(), "cat".into()],
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("cat"));
    assert!(!p.can_act());
}

#[test]
fn supervised_allows_listed_commands() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        allowed_commands: vec!["git".into()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("git status"));
    assert!(!p.is_command_allowed("docker ps"));
}

#[test]
fn full_autonomy_still_respects_forbidden_paths() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed(&tp_sys_sub("etc/shadow")));
    assert!(!p.is_path_allowed(&tp_sys_sub("root/.bashrc")));
}

#[test]
fn workspace_only_false_allows_resolved_outside_workspace() {
    let workspace = std::env::temp_dir().join("zeroclaw_test_ws_only_false");
    let _ = std::fs::create_dir_all(&workspace);
    let canonical_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    let p = SecurityPolicy {
        workspace_dir: canonical_workspace.clone(),
        workspace_only: false,
        forbidden_paths: vec!["/etc".into(), "/var".into()],
        ..SecurityPolicy::default()
    };

    // Path outside workspace should be allowed when workspace_only=false
    let outside = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home"))
        .join("zeroclaw_outside_ws");
    assert!(
        p.is_resolved_path_allowed(&outside),
        "workspace_only=false must allow resolved paths outside workspace"
    );

    // Forbidden paths must still be blocked even with workspace_only=false
    assert!(
        !p.is_resolved_path_allowed(Path::new("/etc/passwd")),
        "forbidden paths must be blocked even when workspace_only=false"
    );
    assert!(
        !p.is_resolved_path_allowed(Path::new("/var/run/docker.sock")),
        "forbidden /var must be blocked even when workspace_only=false"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn workspace_only_true_blocks_resolved_outside_workspace() {
    let workspace = std::env::temp_dir().join("zeroclaw_test_ws_only_true");
    let _ = std::fs::create_dir_all(&workspace);
    let canonical_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    let p = SecurityPolicy {
        workspace_dir: canonical_workspace.clone(),
        workspace_only: true,
        ..SecurityPolicy::default()
    };

    // Path inside workspace — allowed
    let inside = canonical_workspace.join("subdir");
    assert!(
        p.is_resolved_path_allowed(&inside),
        "path inside workspace must be allowed"
    );

    // Path outside workspace — blocked
    let outside = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("zeroclaw_outside_ws_true");
    assert!(
        !p.is_resolved_path_allowed(&outside),
        "workspace_only=true must block resolved paths outside workspace"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

// ── is_resolved_path_readable: read-only allowlist + POSIX devs ──

#[test]
fn readable_includes_posix_device_files() {
    // /dev/null and friends are universally-readable system paths
    // operators expect to work for shell-idiom CLI tooling.
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/tmp/zeroclaw-test-ws"),
        workspace_only: true,
        ..SecurityPolicy::default()
    };
    for device in ["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"] {
        assert!(
            p.is_resolved_path_readable(Path::new(device)),
            "POSIX device file {device} must be readable"
        );
    }
}

#[test]
fn readable_includes_read_only_allowlist_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let read_only_root = tmp.path().join("docs");
    std::fs::create_dir_all(&read_only_root).unwrap();
    let inside = read_only_root.join("guide.md");
    std::fs::write(&inside, "x").unwrap();

    let canonical_inside = inside.canonicalize().unwrap();
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/tmp/elsewhere"),
        workspace_only: true,
        allowed_roots_read_only: vec![read_only_root.clone()],
        ..SecurityPolicy::default()
    };
    assert!(
        p.is_resolved_path_readable(&canonical_inside),
        "read-only allowlist entries must be readable"
    );
    // The same path is NOT writable (is_resolved_path_allowed is
    // strict-rw and does not consult allowed_roots_read_only).
    assert!(
        !p.is_resolved_path_allowed(&canonical_inside),
        "read-only allowlist entries must NOT be writable via is_resolved_path_allowed"
    );
}

// ── for_agent: workspace.access populates allowlist tiers ──

#[test]
fn for_agent_routes_workspace_access_into_correct_allowlist_tier() {
    use crate::multi_agent::{AccessMode, AgentAlias};
    use crate::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    let mut cfg = Config {
        data_dir: PathBuf::from("/tmp/zeroclaw-for-agent-test"),
        config_path: PathBuf::from("/tmp/zeroclaw-for-agent-test/config.toml"),
        ..Config::default()
    };
    cfg.risk_profiles.insert(
        "default".into(),
        RiskProfileConfig {
            workspace_only: true,
            ..RiskProfileConfig::default()
        },
    );

    cfg.agents.insert(
        "writable_sibling".into(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );
    cfg.agents.insert(
        "readonly_sibling".into(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );

    let mut test_agent = AliasedAgentConfig {
        risk_profile: "default".into(),
        ..AliasedAgentConfig::default()
    };
    test_agent
        .workspace
        .access
        .insert(AgentAlias::from("writable_sibling"), AccessMode::Write);
    test_agent
        .workspace
        .access
        .insert(AgentAlias::from("readonly_sibling"), AccessMode::Read);
    cfg.agents.insert("test_agent".into(), test_agent);

    let policy = SecurityPolicy::for_agent(&cfg, "test_agent").unwrap();

    let writable_sibling_dir = cfg.agent_workspace_dir("writable_sibling");
    let readonly_sibling_dir = cfg.agent_workspace_dir("readonly_sibling");

    assert!(
        policy
            .allowed_roots_write_only
            .contains(&writable_sibling_dir),
        "AccessMode::Write must land in allowed_roots_write_only; got {:?}",
        policy.allowed_roots_write_only
    );
    assert!(
        !policy.allowed_roots.contains(&writable_sibling_dir),
        "AccessMode::Write must NOT land in allowed_roots (read+write tier); got {:?}",
        policy.allowed_roots
    );
    assert!(
        policy
            .allowed_roots_read_only
            .contains(&readonly_sibling_dir),
        "AccessMode::Read must land in allowed_roots_read_only; got {:?}",
        policy.allowed_roots_read_only
    );
    assert!(
        !policy
            .allowed_roots_read_only
            .contains(&writable_sibling_dir),
        "Write-mode entry must NOT also appear on the read-only list"
    );
    assert!(
        !policy
            .allowed_roots_write_only
            .contains(&readonly_sibling_dir),
        "Read-mode entry must NOT also appear on the write-only list"
    );
    assert!(
        policy.workspace_only,
        "unrestricted_filesystem stays default-false → workspace_only stays true"
    );
}

#[test]
fn write_only_root_blocks_reads_and_admits_writes() {
    // AccessMode::Write grants write access without read access.
    // is_resolved_path_allowed (write-side) must accept paths under
    // a write-only root; is_resolved_path_readable (read-side) must
    // refuse them.
    let mut policy = SecurityPolicy::default();
    let write_only_root =
        std::env::temp_dir().join(format!("zeroclaw_wo_root_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&write_only_root).unwrap();
    let canonical = write_only_root.canonicalize().unwrap();
    policy.allowed_roots_write_only.push(canonical.clone());
    policy.workspace_only = false;

    let target = canonical.join("write_only_target.txt");
    assert!(
        policy.is_resolved_path_allowed(&target),
        "write-only root must be writable via is_resolved_path_allowed"
    );
    assert!(
        !policy.is_resolved_path_readable(&target),
        "write-only root must NOT be readable via is_resolved_path_readable"
    );

    let _ = std::fs::remove_dir_all(canonical);
}

#[test]
fn for_agent_creates_the_per_agent_workspace_dir() {
    use crate::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    let root =
        std::env::temp_dir().join(format!("zeroclaw-for-agent-mkdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut cfg = Config {
        data_dir: root.join("data"),
        config_path: root.join("config.toml"),
        ..Config::default()
    };
    cfg.risk_profiles
        .insert("default".into(), RiskProfileConfig::default());
    cfg.agents.insert(
        "agent_a".into(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );

    let ws = cfg.agent_workspace_dir("agent_a");
    assert!(
        !ws.exists(),
        "precondition: workspace dir must not exist yet"
    );

    let policy = SecurityPolicy::for_agent(&cfg, "agent_a").unwrap();

    assert!(
        ws.exists(),
        "for_agent must create the per-agent workspace dir at the chokepoint"
    );
    assert_eq!(
        policy.workspace_dir, ws,
        "policy cwd/jail root must be the created workspace dir"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn for_agent_unrestricted_filesystem_disables_workspace_only() {
    use crate::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    let mut cfg = Config {
        data_dir: PathBuf::from("/tmp/zeroclaw-for-agent-unrestricted"),
        config_path: PathBuf::from("/tmp/zeroclaw-for-agent-unrestricted/config.toml"),
        ..Config::default()
    };
    cfg.risk_profiles.insert(
        "default".into(),
        RiskProfileConfig {
            workspace_only: true,
            ..RiskProfileConfig::default()
        },
    );
    let mut test_agent = AliasedAgentConfig {
        risk_profile: "default".into(),
        ..AliasedAgentConfig::default()
    };
    test_agent.workspace.unrestricted_filesystem = true;
    cfg.agents.insert("test_agent".into(), test_agent);

    let policy = SecurityPolicy::for_agent(&cfg, "test_agent").unwrap();

    assert!(
        !policy.workspace_only,
        "unrestricted_filesystem=true must flip workspace_only off at the policy level"
    );
}

// ── Cards: a card-only agent must resolve, and its grants must be the
// sole source of allowed_tools ────────────

/// Builds a `Config` with one `[risk_profiles.<profile_alias>]` entry and
/// one `[cards.<card_alias>]` entry naming it, wired to a single agent
/// defined solely by `card = <card_alias>` (no `risk_profile` set —
/// mirrors what `Config::validate()` requires for a carded agent).
fn carded_agent_config(
    card_alias: &str,
    profile_alias: &str,
    grants: Vec<crate::card::ToolGrant>,
) -> crate::schema::Config {
    carded_agent_config_with_mcp_policy(
        card_alias,
        profile_alias,
        grants,
        crate::autonomy::McpDiscoveredToolPolicy::default(),
    )
}

/// Same shape as [`carded_agent_config`], but lets a test pin the named
/// profile's own `mcp_discovered_tool_policy` — needed to prove the
/// card-governed override forces `ExplicitOnly` regardless of what the
/// profile it points at says.
fn carded_agent_config_with_mcp_policy(
    card_alias: &str,
    profile_alias: &str,
    grants: Vec<crate::card::ToolGrant>,
    mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy,
) -> crate::schema::Config {
    use crate::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    let mut cfg = Config {
        data_dir: PathBuf::from(format!("/tmp/zeroclaw-carded-{card_alias}")),
        config_path: PathBuf::from(format!("/tmp/zeroclaw-carded-{card_alias}/config.toml")),
        ..Config::default()
    };
    cfg.risk_profiles.insert(
        profile_alias.into(),
        RiskProfileConfig {
            mcp_discovered_tool_policy,
            ..RiskProfileConfig::default()
        },
    );
    cfg.cards.insert(
        card_alias.into(),
        crate::card::AgentCard {
            risk_profile: profile_alias.into(),
            grants: crate::card::CardGrants {
                tools: grants,
                ..crate::card::CardGrants::default()
            },
            ..crate::card::AgentCard::default()
        },
    );
    cfg.agents.insert(
        "carded_agent".into(),
        AliasedAgentConfig {
            card: card_alias.into(),
            // risk_profile deliberately left empty — validation forbids
            // setting both card and risk_profile on the same agent.
            ..AliasedAgentConfig::default()
        },
    );
    cfg
}

#[test]
fn a_carded_agent_resolves_and_its_allowed_tools_is_exactly_the_cards_grants() {
    use crate::card::{GrantClass, ToolGrant};

    let cfg = carded_agent_config(
        "trader_card",
        "trading_readonly",
        vec![
            ToolGrant::new("memory_recall", GrantClass::LocalRead),
            ToolGrant::new("hapi-edge__snapshot", GrantClass::LocalRead),
        ],
    );

    let policy = SecurityPolicy::for_agent(&cfg, "carded_agent")
        .expect("a card-only agent must construct a SecurityPolicy");

    assert_eq!(
        policy.allowed_tools,
        Some(vec![
            "memory_recall".to_string(),
            "hapi-edge__snapshot".to_string(),
        ]),
        "allowed_tools must equal exactly the card's granted tool names"
    );
}

#[test]
fn is_tool_allowed_follows_the_cards_grants_exactly() {
    use crate::card::{GrantClass, ToolGrant};

    let cfg = carded_agent_config(
        "trader_card",
        "trading_readonly",
        vec![ToolGrant::new("memory_recall", GrantClass::LocalRead)],
    );
    let policy = SecurityPolicy::for_agent(&cfg, "carded_agent").unwrap();

    assert!(
        policy.is_tool_allowed("memory_recall"),
        "a tool the card grants must be allowed"
    );
    assert!(
        !policy.is_tool_allowed("shell"),
        "a tool the card does not grant must be denied"
    );
}

#[test]
fn a_card_with_no_grants_denies_every_tool_not_unrestricted() {
    let cfg = carded_agent_config("empty_card", "trading_readonly", vec![]);
    let policy = SecurityPolicy::for_agent(&cfg, "carded_agent").unwrap();

    assert_eq!(
        policy.allowed_tools,
        Some(vec![]),
        "an empty grant list must compile to deny-all (Some(vec![])), never to \
         None (unrestricted)"
    );
    assert!(!policy.is_tool_allowed("memory_recall"));
    assert!(!policy.is_tool_allowed("shell"));
}

#[test]
fn an_uncarded_agents_policy_is_unchanged() {
    use crate::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    let mut cfg = Config {
        data_dir: PathBuf::from("/tmp/zeroclaw-uncarded"),
        config_path: PathBuf::from("/tmp/zeroclaw-uncarded/config.toml"),
        ..Config::default()
    };
    cfg.risk_profiles.insert(
        "default".into(),
        RiskProfileConfig {
            allowed_tools: Some(vec!["shell".to_string(), "file_read".to_string()]),
            ..RiskProfileConfig::default()
        },
    );
    cfg.agents.insert(
        "plain_agent".into(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );

    let policy = SecurityPolicy::for_agent(&cfg, "plain_agent").unwrap();

    assert_eq!(
        policy.allowed_tools,
        Some(vec!["shell".to_string(), "file_read".to_string()]),
        "an uncarded agent's allowed_tools must come from its risk_profile, \
         untouched by card resolution — this is the regression guard"
    );
    assert_eq!(
        policy.risk_profile_name, "default",
        "an uncarded agent's risk_profile_name must still be its own direct \
         risk_profile field, not affected by card resolution"
    );
}

#[test]
fn a_carded_agents_mcp_discovered_tool_policy_is_forced_to_explicit_only() {
    use crate::card::{GrantClass, ToolGrant};

    // The named profile deliberately sets the permissive variant: a
    // card cannot mean "auto-admit" because naming is its entire
    // semantics (`CardGrants`' own doc — "there is no 'all'"),
    // regardless of what the profile it points at says.
    let cfg = carded_agent_config_with_mcp_policy(
        "trader_card",
        "trading_readonly",
        vec![ToolGrant::new("memory_recall", GrantClass::LocalRead)],
        crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit,
    );

    let policy = SecurityPolicy::for_agent(&cfg, "carded_agent").unwrap();

    assert_eq!(
        policy.mcp_discovered_tool_policy,
        crate::autonomy::McpDiscoveredToolPolicy::ExplicitOnly,
        "a card governs this agent, so the profile's AutoAdmit escape \
         hatch must be closed regardless"
    );
}

#[test]
fn an_uncarded_agents_mcp_discovered_tool_policy_keeps_auto_admit() {
    use crate::schema::{AliasedAgentConfig, Config, RiskProfileConfig};

    // Regression guard: no card governs this agent, so `for_agent` must
    // not touch `mcp_discovered_tool_policy` at all — it stays whatever
    // the profile itself set.
    let mut cfg = Config {
        data_dir: PathBuf::from("/tmp/zeroclaw-uncarded-mcp"),
        config_path: PathBuf::from("/tmp/zeroclaw-uncarded-mcp/config.toml"),
        ..Config::default()
    };
    cfg.risk_profiles.insert(
        "default".into(),
        RiskProfileConfig {
            mcp_discovered_tool_policy: crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit,
            ..RiskProfileConfig::default()
        },
    );
    cfg.agents.insert(
        "plain_agent".into(),
        AliasedAgentConfig {
            risk_profile: "default".into(),
            ..AliasedAgentConfig::default()
        },
    );

    let policy = SecurityPolicy::for_agent(&cfg, "plain_agent").unwrap();

    assert_eq!(
        policy.mcp_discovered_tool_policy,
        crate::autonomy::McpDiscoveredToolPolicy::AutoAdmit,
        "an uncarded agent's mcp_discovered_tool_policy must come from its \
         risk_profile, untouched by card resolution"
    );
}

// ── Edge cases: from_config preserves tracker ────────────

#[test]
fn from_config_creates_fresh_tracker() {
    let risk = crate::schema::RiskProfileConfig {
        level: AutonomyLevel::Full,
        workspace_only: false,
        allowed_commands: vec![],
        forbidden_paths: vec![],
        require_approval_for_medium_risk: true,
        block_high_risk_commands: true,
        ..crate::schema::RiskProfileConfig::default()
    };
    let runtime = crate::schema::RuntimeProfileConfig {
        max_actions_per_hour: 10,
        max_cost_per_day_cents: 100,
        ..crate::schema::RuntimeProfileConfig::default()
    };
    let workspace = PathBuf::from("/tmp/test");
    let policy = SecurityPolicy::from_profiles(&risk, Some(&runtime), &workspace);
    assert!(!policy.is_rate_limited());
}

// ── Checklist #3: Filesystem scoped (no /) ──────────────

#[test]
fn checklist_root_path_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed(tp_sys()));
    assert!(!p.is_path_allowed(&tp_sys_sub("anything")));
}

#[test]
fn checklist_all_system_dirs_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    #[cfg(not(target_os = "windows"))]
    {
        for dir in ["/etc", "/root", "/proc", "/sys", "/dev", "/var", "/tmp"] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dir),
                "Default forbidden_paths must include {dir} on Unix"
            );
            assert!(
                !p.is_path_allowed(dir),
                "System dir should be blocked: {dir}"
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        for dir in [
            "C:\\Windows",
            "C:\\Windows\\System32",
            "C:\\Program Files",
            "C:\\ProgramData",
        ] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dir),
                "Default forbidden_paths must include {dir} on Windows"
            );
            assert!(
                !p.is_path_allowed(dir),
                "System dir should be blocked: {dir}"
            );
        }
    }
    for dot in &["~/.ssh", "~/.gnupg", "~/.aws"] {
        assert!(
            p.forbidden_paths.iter().any(|f| f == dot),
            "Default forbidden_paths must include {dot}"
        );
        assert!(
            !p.is_path_allowed(dot),
            "Sensitive dotfile dir should be blocked: {dot}"
        );
    }
}

#[test]
fn checklist_sensitive_dotfiles_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    for path in [
        "~/.ssh/id_rsa",
        "~/.gnupg/secring.gpg",
        "~/.aws/credentials",
        "~/.config/secrets",
    ] {
        assert!(
            !p.is_path_allowed(path),
            "Sensitive dotfile should be blocked: {path}"
        );
    }
}

#[test]
fn checklist_null_byte_injection_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("safe\0/../../../etc/passwd"));
    assert!(!p.is_path_allowed("\0"));
    assert!(!p.is_path_allowed("file\0"));
}

#[test]
fn checklist_workspace_only_blocks_absolute_outside_workspace() {
    let p = SecurityPolicy {
        workspace_only: true,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed(&tp_sys_sub("any/absolute/path")));
    assert!(p.is_path_allowed("relative/path.txt"));
}

#[test]
fn checklist_resolved_path_must_be_in_workspace() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/home/user/project"),
        ..SecurityPolicy::default()
    };
    // Inside workspace — allowed
    assert!(p.is_resolved_path_allowed(Path::new("/home/user/project/src/main.rs")));
    // Outside workspace — blocked (symlink escape)
    assert!(!p.is_resolved_path_allowed(Path::new("/etc/passwd")));
    assert!(!p.is_resolved_path_allowed(Path::new("/home/user/other_project/file")));
    // Root — blocked
    assert!(!p.is_resolved_path_allowed(Path::new("/")));
}

#[test]
fn checklist_default_policy_is_workspace_only() {
    let p = SecurityPolicy::default();
    assert!(
        p.workspace_only,
        "Default policy must be workspace_only=true"
    );
}

#[test]
fn checklist_default_forbidden_paths_comprehensive() {
    let p = SecurityPolicy::default();
    #[cfg(not(target_os = "windows"))]
    {
        for dir in ["/etc", "/root", "/proc", "/sys", "/dev", "/var", "/tmp"] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dir),
                "Default forbidden_paths must include {dir} on Unix"
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        for dir in [
            "C:\\Windows",
            "C:\\Windows\\System32",
            "C:\\Program Files",
            "C:\\ProgramData",
        ] {
            assert!(
                p.forbidden_paths.iter().any(|f| f == dir),
                "Default forbidden_paths must include {dir} on Windows"
            );
        }
    }
    for dot in &["~/.ssh", "~/.gnupg", "~/.aws", "~/.config"] {
        assert!(
            p.forbidden_paths.iter().any(|f| f == dot),
            "Default forbidden_paths must include {dot}"
        );
    }
}

// ── §1.2 Path resolution / symlink bypass tests ──────────

#[test]
fn resolved_path_blocks_outside_workspace() {
    let workspace = std::env::temp_dir().join("zeroclaw_test_resolved_path");
    let _ = std::fs::create_dir_all(&workspace);

    // Use the canonicalized workspace so starts_with checks match
    let canonical_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    let policy = SecurityPolicy {
        workspace_dir: canonical_workspace.clone(),
        ..SecurityPolicy::default()
    };

    // A resolved path inside the workspace should be allowed
    let inside = canonical_workspace.join("subdir").join("file.txt");
    assert!(
        policy.is_resolved_path_allowed(&inside),
        "path inside workspace should be allowed"
    );

    // A resolved path outside the workspace should be blocked
    let canonical_temp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    let outside = canonical_temp.join("outside_workspace_zeroclaw");
    assert!(
        !policy.is_resolved_path_allowed(&outside),
        "path outside workspace must be blocked"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn resolved_path_blocks_root_escape() {
    let policy = SecurityPolicy {
        workspace_dir: PathBuf::from("/home/zeroclaw_user/project"),
        ..SecurityPolicy::default()
    };

    assert!(
        !policy.is_resolved_path_allowed(Path::new("/etc/passwd")),
        "resolved path to /etc/passwd must be blocked"
    );
    assert!(
        !policy.is_resolved_path_allowed(Path::new("/root/.bashrc")),
        "resolved path to /root/.bashrc must be blocked"
    );
}

#[cfg(unix)]
#[test]
fn resolved_path_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join("zeroclaw_test_symlink_escape");
    let workspace = root.join("workspace");
    let outside = root.join("outside_target");

    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    // Create a symlink inside workspace pointing outside
    let link_path = workspace.join("escape_link");
    symlink(&outside, &link_path).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    // The resolved symlink target should be outside workspace
    let resolved = link_path.canonicalize().unwrap();
    assert!(
        !policy.is_resolved_path_allowed(&resolved),
        "symlink-resolved path outside workspace must be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Regression for the shell workspace-boundary bypass: a direct path-shaped
/// command argument that reaches outside the workspace through an
/// in-workspace symlink must be blocked. The leaf may not exist yet (the
/// command is about to create it), so resolution must follow the symlinked
/// ancestor.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_symlink_escape_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let outside = root.join("outside_target");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    // `link` inside the workspace points at the outside directory.
    symlink(&outside, workspace.join("link")).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    // Writing a NEW file through the symlink (leaf does not exist yet).
    assert_eq!(
        policy
            .forbidden_workspace_path_argument("touch link/new.txt")
            .as_deref(),
        Some("link/new.txt"),
        "creating a file through an in-workspace symlink to outside must be blocked"
    );
    // Redirect target through the symlink.
    assert!(
        policy
            .forbidden_workspace_path_argument("echo hi > link/out.txt")
            .is_some(),
        "shell redirect through an escaping symlink must be blocked"
    );
    // Reading an existing file through the symlink.
    std::fs::write(outside.join("secret.txt"), b"x").unwrap();
    assert!(
        policy
            .forbidden_workspace_path_argument("cat link/secret.txt")
            .is_some(),
        "reading through an escaping symlink must be blocked"
    );

    // A DANGLING symlink (its target directory does not exist yet) still
    // escapes on write, so it must be blocked even though it cannot be
    // `canonicalize`d.
    symlink(outside.join("nonexistent_dir"), workspace.join("dangling")).unwrap();
    assert!(
        policy
            .forbidden_workspace_path_argument("echo x > dangling/new.txt")
            .is_some(),
        "writing through a dangling symlink to outside must be blocked"
    );

    // A symlink CYCLE exhausts the resolver's hop budget. It must fail
    // CLOSED (block), not fall back to the pristine in-workspace path.
    symlink(workspace.join("cycle_b"), workspace.join("cycle_a")).unwrap();
    symlink(workspace.join("cycle_a"), workspace.join("cycle_b")).unwrap();
    assert!(
        policy
            .forbidden_workspace_path_argument("cat cycle_a/file")
            .is_some(),
        "an unresolvable symlink cycle must fail closed (be blocked)"
    );

    // Scoping check: the STRING-only guard (used by cron, whose cwd is NOT
    // the workspace) must NOT resolve symlinks, so it does not over-block a
    // workspace-relative path here. The resolve step is scoped to the
    // workspace-cwd variant used above.
    assert_eq!(
        policy.forbidden_path_argument("cat link/secret.txt"),
        None,
        "string-only guard must not resolve symlinks (keeps cron unaffected)"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Relative symlink target (`link -> ../outside`) must still be blocked.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_blocks_relative_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_rel_symlink_escape_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"x").unwrap();

    symlink(Path::new("../outside"), workspace.join("link")).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    assert!(
        policy
            .forbidden_workspace_path_argument("cat link/secret.txt")
            .is_some(),
        "relative symlink escape (link -> ../outside) must be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Intermediate-component symlink (`a/b/link/x`) must be followed and blocked.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_blocks_nested_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_nested_symlink_escape_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(workspace.join("a/b")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x"), b"x").unwrap();

    symlink(&outside, workspace.join("a/b/link")).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    assert!(
        policy
            .forbidden_workspace_path_argument("cat a/b/link/x")
            .is_some(),
        "nested intermediate symlink escape must be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `./link/x` is path-shaped (starts with `./`) and must resolve the symlink.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_blocks_dot_slash_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_dot_slash_symlink_escape_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x"), b"x").unwrap();

    symlink(&outside, workspace.join("link")).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    assert!(
        policy
            .forbidden_workspace_path_argument("cat ./link/x")
            .is_some(),
        "./link/x symlink escape must be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Known limitation (not a bug): bare-name tokens without a path separator
/// are not recognized as paths by `looks_like_path`, so a symlink named
/// `link` used as `cat link` is NOT blocked by this static scan. Guard this
/// so a future accidental semantic change is visible.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_does_not_scan_bare_name_symlink_known_limitation() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_bare_name_symlink_limit_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"x").unwrap();

    // Symlink leaf that would escape if resolved — but the token has no
    // path separator, so the scanner never treats it as a path.
    symlink(outside.join("secret.txt"), workspace.join("link")).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    assert_eq!(
        policy.forbidden_workspace_path_argument("cat link"),
        None,
        "known limitation: bare-name symlink tokens are not path-scanned"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Non-regression: a symlink that stays INSIDE the workspace, and ordinary
/// workspace-relative paths, must still be allowed after the resolve check.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_allows_in_workspace_paths() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_in_workspace_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(workspace.join("real_dir")).unwrap();
    std::fs::create_dir_all(workspace.join("sub")).unwrap();

    // `inside` points at another directory WITHIN the workspace.
    symlink(workspace.join("real_dir"), workspace.join("inside")).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };

    assert_eq!(
        policy.forbidden_workspace_path_argument("touch inside/new.txt"),
        None,
        "an in-workspace symlink target must remain allowed"
    );
    assert_eq!(
        policy.forbidden_workspace_path_argument("touch sub/out.txt"),
        None,
        "an ordinary workspace-relative path must remain allowed"
    );
    assert_eq!(
        policy.forbidden_workspace_path_argument("echo hi > sub/out.txt"),
        None,
        "an ordinary workspace-relative redirect target must remain allowed"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Non-regression: a path under an `allowed_roots_read_only` grant (outside
/// the workspace) must stay READABLE - the command guard checks read OR
/// write, so the resolve step must not drop read-only roots.
#[cfg(unix)]
#[test]
fn forbidden_path_argument_allows_read_only_root() {
    let root = std::env::temp_dir().join(format!(
        "zeroclaw_test_shell_read_only_root_{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let shared = root.join("shared_readonly");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("data.txt"), b"x").unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        allowed_roots_read_only: vec![shared.clone()],
        workspace_only: true,
        ..SecurityPolicy::default()
    };

    let cmd = format!("cat {}/data.txt", shared.display());
    assert_eq!(
        policy.forbidden_workspace_path_argument(&cmd),
        None,
        "reading under an allowed read-only root must remain allowed"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn allowed_roots_permits_paths_outside_workspace() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join("zeroclaw_test_allowed_roots");
    let workspace = root.join("workspace");
    let extra = root.join("extra_root");
    let extra_file = extra.join("data.txt");

    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(&extra_file, "test").unwrap();

    // Symlink inside workspace pointing to extra root
    let link_path = workspace.join("link_to_extra");
    symlink(&extra, &link_path).unwrap();

    let resolved = link_path.join("data.txt").canonicalize().unwrap();

    // Without allowed_roots — blocked (symlink escape)
    let policy_without = SecurityPolicy {
        workspace_dir: workspace.clone(),
        allowed_roots: vec![],
        ..SecurityPolicy::default()
    };
    assert!(
        !policy_without.is_resolved_path_allowed(&resolved),
        "without allowed_roots, symlink target must be blocked"
    );

    // With allowed_roots — permitted
    let policy_with = SecurityPolicy {
        workspace_dir: workspace.clone(),
        allowed_roots: vec![extra.clone()],
        ..SecurityPolicy::default()
    };
    assert!(
        policy_with.is_resolved_path_allowed(&resolved),
        "with allowed_roots containing the target, symlink must be allowed"
    );

    // Unrelated path still blocked
    let unrelated = root.join("unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    assert!(
        !policy_with.is_resolved_path_allowed(&unrelated.canonicalize().unwrap()),
        "paths outside workspace and allowed_roots must still be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn is_path_allowed_blocks_null_bytes() {
    let policy = default_policy();
    assert!(
        !policy.is_path_allowed("file\0.txt"),
        "paths with null bytes must be blocked"
    );
}

#[test]
fn is_path_allowed_blocks_url_encoded_traversal() {
    let policy = default_policy();
    assert!(
        !policy.is_path_allowed("..%2fetc%2fpasswd"),
        "URL-encoded path traversal must be blocked"
    );
    assert!(
        !policy.is_path_allowed("subdir%2f..%2f..%2fetc"),
        "URL-encoded parent dir traversal must be blocked"
    );
}

#[test]
fn resolve_tool_path_expands_tilde() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/workspace"),
        ..SecurityPolicy::default()
    };
    let resolved = p.resolve_tool_path("~/Documents/file.txt");
    // Should expand ~ to home dir, not join with workspace
    assert!(resolved.is_absolute());
    assert!(!resolved.starts_with("/workspace"));
    assert!(resolved.to_string_lossy().ends_with("Documents/file.txt"));
}

#[test]
fn resolve_tool_path_keeps_absolute() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/workspace"),
        ..SecurityPolicy::default()
    };
    let resolved = p.resolve_tool_path("/some/absolute/path");
    assert_eq!(resolved, PathBuf::from("/some/absolute/path"));
}

#[test]
fn resolve_tool_path_joins_relative() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/workspace"),
        ..SecurityPolicy::default()
    };
    let resolved = p.resolve_tool_path("relative/path.txt");
    assert_eq!(resolved, PathBuf::from("/workspace/relative/path.txt"));
}

#[test]
fn resolve_tool_path_normalizes_workspace_prefixed_relative_paths() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/zeroclaw-data/workspace"),
        ..SecurityPolicy::default()
    };
    let resolved = p.resolve_tool_path("zeroclaw-data/workspace/scripts/daily.py");
    assert_eq!(
        resolved,
        PathBuf::from("/zeroclaw-data/workspace/scripts/daily.py")
    );
}

#[test]
fn resolve_tool_path_normalizes_windows_workspace_prefixed_relative_paths() {
    let workspace = PathBuf::from(r"C:\Users\me\.zeroclaw\agents\default\workspace");
    let p = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    let resolved =
        p.resolve_tool_path(r"Users\me\.zeroclaw\agents\default\workspace\nested\out.txt");

    assert_eq!(resolved, workspace.join("nested").join("out.txt"));
}

#[test]
fn resolve_tool_path_does_not_normalize_mismatched_drive_prefixed_relative_paths() {
    let workspace = PathBuf::from(r"C:\Users\me\.zeroclaw\agents\default\workspace");
    let p = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    let resolved =
        p.resolve_tool_path(r"D:Users\me\.zeroclaw\agents\default\workspace\nested\out.txt");

    assert_ne!(resolved, workspace.join("nested").join("out.txt"));
}

#[test]
fn is_under_allowed_root_matches_allowed_roots() {
    let p = SecurityPolicy {
        workspace_dir: tp_ws(),
        workspace_only: true,
        allowed_roots: vec![tp_proj(), tp_data()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_under_allowed_root(&format!("{}/myapp/src/main.rs", tp_proj().display())));
    assert!(p.is_under_allowed_root(&format!("{}/file.csv", tp_data().display())));
    assert!(!p.is_under_allowed_root(&tp_sys_sub("etc/passwd")));
    assert!(!p.is_under_allowed_root("relative/path"));
}

#[test]
fn is_under_allowed_root_returns_false_for_empty_roots() {
    let p = SecurityPolicy {
        workspace_dir: tp_ws(),
        workspace_only: true,
        allowed_roots: vec![],
        ..SecurityPolicy::default()
    };
    assert!(!p.is_under_allowed_root(&format!("{}/any/path", tp_proj().display())));
}

// ── SecurityPolicy read/read-write split ────────────────────────

#[test]
fn is_under_read_only_allowed_root_matches_only_read_only_list() {
    let p = SecurityPolicy {
        workspace_dir: tp_ws(),
        workspace_only: true,
        allowed_roots: vec![tp_rw()],
        allowed_roots_read_only: vec![tp_ro()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_under_read_only_allowed_root(&format!("{}/notes.md", tp_ro().display())));
    assert!(!p.is_under_read_only_allowed_root(&format!("{}/file.csv", tp_rw().display())));
    assert!(!p.is_under_read_only_allowed_root(&tp_sys_sub("etc/passwd")));
    assert!(!p.is_under_read_only_allowed_root("relative"));
}

#[test]
fn is_under_any_allowed_root_unions_read_only_and_read_write() {
    let p = SecurityPolicy {
        workspace_dir: tp_ws(),
        workspace_only: true,
        allowed_roots: vec![tp_rw()],
        allowed_roots_read_only: vec![tp_ro()],
        ..SecurityPolicy::default()
    };
    assert!(p.is_under_any_allowed_root(&format!("{}/file.csv", tp_rw().display())));
    assert!(p.is_under_any_allowed_root(&format!("{}/notes.md", tp_ro().display())));
    assert!(!p.is_under_any_allowed_root(&tp_sys_sub("etc/passwd")));
}

#[test]
fn is_under_allowed_root_does_not_see_read_only_entries() {
    let p = SecurityPolicy {
        workspace_dir: tp_ws(),
        workspace_only: true,
        allowed_roots: vec![],
        allowed_roots_read_only: vec![tp_ro()],
        ..SecurityPolicy::default()
    };
    assert!(!p.is_under_allowed_root(&format!("{}/notes.md", tp_ro().display())));
    assert!(p.is_under_any_allowed_root(&format!("{}/notes.md", tp_ro().display())));
}

// ── SubAgent escalation validator ──────────────────────────────

fn parent_policy_for_escalation_tests() -> SecurityPolicy {
    SecurityPolicy {
        workspace_dir: PathBuf::from("/workspace"),
        workspace_only: true,
        allowed_roots: vec![PathBuf::from("/projects"), PathBuf::from("/data")],
        allowed_roots_read_only: vec![PathBuf::from("/shared-docs")],
        allowed_commands: vec!["git".into(), "cargo".into(), "ls".into()],
        max_actions_per_hour: 100,
        max_cost_per_day_cents: 500,
        ..SecurityPolicy::default()
    }
}

#[test]
fn ensure_no_escalation_accepts_identical_policy() {
    let parent = parent_policy_for_escalation_tests();
    let child = parent.clone();
    assert!(child.ensure_no_escalation_beyond(&parent).is_ok());
}

#[test]
fn ensure_no_escalation_accepts_narrowed_child() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        allowed_roots: vec![PathBuf::from("/projects")],
        allowed_roots_read_only: vec![PathBuf::from("/shared-docs")],
        allowed_commands: vec!["git".into()],
        max_actions_per_hour: 50,
        max_cost_per_day_cents: 250,
        ..parent.clone()
    };
    assert!(child.ensure_no_escalation_beyond(&parent).is_ok());
}

#[test]
fn ensure_no_escalation_accepts_case_equivalent_command_names() {
    let parent = SecurityPolicy {
        allowed_commands: vec!["Git".into(), "DOCKER".into()],
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        allowed_commands: vec!["git".into(), "docker".into()],
        ..parent.clone()
    };

    assert!(child.ensure_no_escalation_beyond(&parent).is_ok());
}

#[test]
fn ensure_no_escalation_keeps_command_paths_case_sensitive() {
    let parent = SecurityPolicy {
        allowed_commands: vec!["/usr/bin/Git".into()],
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        allowed_commands: vec!["/usr/bin/git".into()],
        ..parent.clone()
    };

    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("case-distinct paths must not be treated as equivalent");
    assert!(matches!(
        err,
        EscalationViolation::CommandNotInParent { ref command }
        if command == "/usr/bin/git"
    ));
}

#[test]
fn ensure_no_escalation_accepts_rw_root_downgraded_to_read_only_on_child() {
    // A SubAgent giving up its write privilege is a narrowing,
    // not an escalation.
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        allowed_roots: Vec::new(),
        allowed_roots_read_only: vec![PathBuf::from("/projects")],
        ..parent.clone()
    };
    assert!(child.ensure_no_escalation_beyond(&parent).is_ok());
}

#[test]
fn ensure_no_escalation_rejects_new_rw_root_not_in_parent() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        allowed_roots: vec![PathBuf::from("/projects"), PathBuf::from("/secrets")],
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("new rw root must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::ReadWriteRootNotInParent { ref path }
        if path == &PathBuf::from("/secrets")
    ));
}

#[test]
fn ensure_no_escalation_rejects_new_read_only_root_not_in_parent() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        allowed_roots_read_only: vec![PathBuf::from("/etc")],
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("new read-only root must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::ReadOnlyRootNotInParent { ref path }
        if path == &PathBuf::from("/etc")
    ));
}

#[test]
fn ensure_no_escalation_rejects_new_command_not_in_parent() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        allowed_commands: vec!["git".into(), "rm".into()],
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("new command must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::CommandNotInParent { ref command }
        if command == "rm"
    ));
}

#[test]
fn ensure_no_escalation_rejects_workspace_only_disabled_by_child() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        workspace_only: false,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("disabling workspace_only when parent enforces it must be rejected");
    assert_eq!(err, EscalationViolation::WorkspaceOnlyDisabledByChild);
}

#[test]
fn ensure_no_escalation_rejects_higher_max_actions() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        max_actions_per_hour: 200,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("higher max_actions_per_hour must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::MaxActionsExceeded { child, parent } if child == 200 && parent == 100
    ));
}

#[test]
fn ensure_no_escalation_rejects_higher_max_cost() {
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        max_cost_per_day_cents: 1000,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("higher max_cost_per_day_cents must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::MaxCostExceeded { child, parent } if child == 1000 && parent == 500
    ));
}

#[test]
fn ensure_no_escalation_rejects_higher_autonomy() {
    let parent = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("Full child under Supervised parent must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::AutonomyAboveParent { child, parent }
        if child == AutonomyLevel::Full && parent == AutonomyLevel::Supervised
    ));
}

#[test]
fn ensure_no_escalation_accepts_subpath_narrowing_inside_parent_root() {
    // Parent grants /projects rw; child narrows to /projects/repo —
    // a containment relation, not exact equality. Must accept.
    let parent = parent_policy_for_escalation_tests();
    let child = SecurityPolicy {
        allowed_roots: vec![PathBuf::from("/projects/repo")],
        allowed_roots_read_only: vec![],
        ..parent.clone()
    };
    assert!(child.ensure_no_escalation_beyond(&parent).is_ok());
}

#[test]
fn ensure_no_escalation_rejects_dropped_forbidden_path() {
    let parent = SecurityPolicy {
        forbidden_paths: vec!["/etc/secrets".into(), "/root".into()],
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        forbidden_paths: vec!["/root".into()],
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("child dropping a parent's forbidden_paths entry must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::ForbiddenPathDroppedByChild { ref path }
        if path == "/etc/secrets"
    ));
}

#[test]
fn ensure_no_escalation_rejects_expanded_shell_env_passthrough() {
    let parent = SecurityPolicy {
        shell_env_passthrough: vec!["PATH".into()],
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        shell_env_passthrough: vec!["PATH".into(), "AWS_SECRET_ACCESS_KEY".into()],
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("child adding a shell_env_passthrough entry must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::ShellEnvPassthroughExpanded { ref variable }
        if variable == "AWS_SECRET_ACCESS_KEY"
    ));
}

#[test]
fn ensure_no_escalation_rejects_higher_shell_timeout() {
    let parent = SecurityPolicy {
        shell_timeout_secs: 30,
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        shell_timeout_secs: 600,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("higher shell_timeout_secs must be rejected");
    assert!(matches!(
        err,
        EscalationViolation::ShellTimeoutExceeded { child, parent }
        if child == 600 && parent == 30
    ));
}

#[test]
fn ensure_no_escalation_rejects_disabled_block_high_risk_commands() {
    let parent = SecurityPolicy {
        block_high_risk_commands: true,
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        block_high_risk_commands: false,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("child flipping block_high_risk_commands off must be rejected");
    assert_eq!(
        err,
        EscalationViolation::BlockHighRiskCommandsDisabledByChild
    );
}

#[test]
fn ensure_no_escalation_rejects_disabled_require_approval() {
    let parent = SecurityPolicy {
        require_approval_for_medium_risk: true,
        ..parent_policy_for_escalation_tests()
    };
    let child = SecurityPolicy {
        require_approval_for_medium_risk: false,
        ..parent.clone()
    };
    let err = child
        .ensure_no_escalation_beyond(&parent)
        .expect_err("child flipping require_approval_for_medium_risk off must be rejected");
    assert_eq!(err, EscalationViolation::RequireApprovalDisabledByChild);
}

#[test]
fn from_risk_profile_leaves_allowed_roots_read_only_empty() {
    // RiskProfileConfig has no read-only-roots concept; it's
    // populated by the multi-agent runtime when it builds the
    // per-agent policy from workspace.access.
    let profile = crate::schema::RiskProfileConfig {
        allowed_roots: vec!["/projects".to_string()],
        ..crate::schema::RiskProfileConfig::default()
    };
    let policy = SecurityPolicy::from_risk_profile(&profile, Path::new("/workspace"));
    assert_eq!(policy.allowed_roots, vec![PathBuf::from("/projects")]);
    assert!(
        policy.allowed_roots_read_only.is_empty(),
        "read-only roots come from workspace.access, not RiskProfileConfig"
    );
}

#[test]
fn runtime_config_paths_are_protected() {
    let workspace = PathBuf::from("/tmp/zeroclaw-profile/workspace");
    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    let config_dir = workspace.parent().unwrap();

    assert!(policy.is_runtime_config_path(&config_dir.join("config.toml")));
    assert!(policy.is_runtime_config_path(&config_dir.join("config.toml.bak")));
    assert!(policy.is_runtime_config_path(&config_dir.join(".config.toml.tmp-1234")));
    // The active_workspace.toml marker file was retired with the
    // [workspace] block; protection is no longer required and not
    // claimed.
    assert!(!policy.is_runtime_config_path(&config_dir.join("active_workspace.toml")));
}

#[test]
fn runtime_state_files_in_config_dir_are_protected() {
    let workspace = PathBuf::from("/tmp/zeroclaw-profile/workspace");
    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    let config_dir = workspace.parent().unwrap();

    // The state files are protected when they live in a runtime config dir.
    assert!(policy.is_runtime_config_path(&config_dir.join("estop-state.json")));
    assert!(policy.is_runtime_config_path(&config_dir.join("webauthn_credentials.json")));
    assert!(policy.is_runtime_config_path(&config_dir.join("otp-secret")));

    // Same names inside the workspace itself are NOT protected — those
    // are user-owned files; only the runtime-state files at config_dir
    // are sensitive.
    assert!(!policy.is_runtime_config_path(&workspace.join("estop-state.json")));
    assert!(!policy.is_runtime_config_path(&workspace.join("webauthn_credentials.json")));
    assert!(!policy.is_runtime_config_path(&workspace.join("otp-secret")));

    // Unrelated filenames are unaffected.
    assert!(!policy.is_runtime_config_path(&config_dir.join("notes.md")));
    assert!(!policy.is_runtime_config_path(&config_dir.join("agent-state.json")));
    assert!(!policy.is_runtime_config_path(&config_dir.join("otp-state.json")));
}

#[test]
fn runtime_state_files_in_data_dir_are_protected() {
    let workspace = PathBuf::from("/tmp/zeroclaw-profile/workspace");
    let data_dir = PathBuf::from("/tmp/zeroclaw-profile/data");
    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        data_dir: Some(data_dir.clone()),
        ..SecurityPolicy::default()
    };

    // The WebAuthn credentials file under data_dir is protected.
    assert!(policy.is_runtime_config_path(&data_dir.join("webauthn_credentials.json")));

    // The same file under the workspace is not — that's a user-owned
    // file, not a WebAuthn store.
    assert!(!policy.is_runtime_config_path(&workspace.join("webauthn_credentials.json")));

    // Unrelated files under data_dir are unaffected.
    assert!(!policy.is_runtime_config_path(&data_dir.join("logs.txt")));
    assert!(!policy.is_runtime_config_path(&data_dir.join("cache.bin")));

    // Without `data_dir` set, the predicate falls back to the
    // config-dir-only behavior (matches the legacy flat layout where
    // everything lives under one tree).
    let legacy_policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    assert!(!legacy_policy.is_runtime_config_path(&PathBuf::from(
        "/tmp/zeroclaw-profile/data/webauthn_credentials.json"
    )));
}

#[test]
fn workspace_files_are_not_runtime_config_paths() {
    let workspace = PathBuf::from("/tmp/zeroclaw-profile/workspace");
    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    let nested_dir = workspace.join("notes");

    assert!(!policy.is_runtime_config_path(&workspace.join("notes.txt")));
    assert!(!policy.is_runtime_config_path(&nested_dir.join("config.toml")));
}

#[test]
fn is_runtime_config_path_protects_install_root_for_nested_agent_layout() {
    let install_root = PathBuf::from("/tmp/zeroclaw-install-nested");
    let workspace = install_root
        .join("agents")
        .join("agent-alpha")
        .join("workspace");
    let config_path = install_root.join("config.toml");

    let policy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        config_path: Some(config_path.clone()),
        ..SecurityPolicy::default()
    };

    // The real config at the install root is now protected.
    assert!(policy.is_runtime_config_path(&config_path));
    assert!(policy.is_runtime_config_path(&install_root.join("config.toml.bak")));
    assert!(policy.is_runtime_config_path(&install_root.join(".config.toml.tmp-42")));

    // Legacy fallback (config_path = None) only guards
    // `workspace.parent()`, so it misses the nested install-root
    // config. This is exactly the gap the config_path field closes.
    let legacy = SecurityPolicy {
        workspace_dir: workspace.clone(),
        ..SecurityPolicy::default()
    };
    assert!(!legacy.is_runtime_config_path(&config_path));

    // A config.toml inside the agent workspace is still not a runtime
    // config path under either policy.
    assert!(!policy.is_runtime_config_path(&workspace.join("config.toml")));
    assert!(!legacy.is_runtime_config_path(&workspace.join("config.toml")));
}

// ── prompt_summary ──────────────────────────────────────

#[test]
fn prompt_summary_includes_autonomy_level() {
    let p = default_policy();
    let summary = p.prompt_summary();
    assert!(
        summary.contains("Supervised"),
        "should mention autonomy level"
    );
}

#[test]
fn prompt_summary_includes_workspace_boundary_when_workspace_only() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/home/user/project"),
        workspace_only: true,
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(
        summary.contains("Workspace boundary"),
        "should mention workspace boundary"
    );
    assert!(
        summary.contains("/home/user/project"),
        "should mention workspace path"
    );
}

#[test]
fn prompt_summary_omits_workspace_boundary_when_not_workspace_only() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(
        !summary.contains("Workspace boundary"),
        "should not mention workspace boundary"
    );
}

#[test]
fn prompt_summary_includes_allowed_commands() {
    let p = SecurityPolicy {
        allowed_commands: vec!["git".into(), "ls".into()],
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(summary.contains("`git`"), "should list allowed commands");
    assert!(summary.contains("`ls`"), "should list allowed commands");
    assert!(
        summary.contains("You may execute these commands freely"),
        "should mention allowed commands positively"
    );
}

#[test]
fn prompt_summary_includes_forbidden_paths() {
    let p = SecurityPolicy {
        workspace_only: false,
        forbidden_paths: vec!["/etc".into(), "~/.ssh".into()],
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(summary.contains("`/etc`"), "should list forbidden paths");
    assert!(summary.contains("`~/.ssh`"), "should list forbidden paths");
}

#[test]
fn prompt_summary_includes_rate_limit() {
    let p = SecurityPolicy {
        max_actions_per_hour: 42,
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(summary.contains("42"), "should mention rate limit");
    assert!(
        summary.contains("actions per hour"),
        "should explain rate limit"
    );
}

#[test]
fn prompt_summary_includes_risk_controls() {
    let p = SecurityPolicy {
        block_high_risk_commands: true,
        require_approval_for_medium_risk: true,
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(
        summary.contains("Exercise caution with destructive commands"),
        "should mention high-risk caution"
    );
    assert!(
        summary.contains("Medium-risk commands"),
        "should mention medium-risk approval"
    );
}

#[test]
fn prompt_summary_includes_allowed_roots() {
    let p = SecurityPolicy {
        allowed_roots: vec![PathBuf::from("/shared/data"), PathBuf::from("/opt/tools")],
        ..SecurityPolicy::default()
    };
    let summary = p.prompt_summary();
    assert!(
        summary.contains("`/shared/data`"),
        "should list allowed roots"
    );
    assert!(
        summary.contains("`/opt/tools`"),
        "should list allowed roots"
    );
}

#[test]
fn wildcard_with_block_high_risk_false_allows_everything() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(
        p.validate_command_execution("rm -rf /tmp/test", true)
            .is_ok()
    );
    assert!(p.validate_command_execution("nohup firefox", true).is_ok());
    assert!(
        p.validate_command_execution("ls /usr/bin/firefox", true)
            .is_ok()
    );
}

#[test]
fn wildcard_with_block_high_risk_true_still_blocks() {
    // Ensure the existing safety net is preserved: wildcard + block_high_risk_commands=true
    // should still block high-risk commands.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };
    let result = p.validate_command_execution("rm -rf /tmp/test", true);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("high-risk"));
}

// ── Shell guard bypass with wildcard + unblocked ──────────

#[test]
fn wildcard_unblocked_allows_backticks() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("echo `whoami`"));
    assert!(p.is_command_allowed("ls `which git`"));
}

#[test]
fn wildcard_unblocked_allows_dollar_paren() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("echo $(cat /etc/hostname)"));
    assert!(p.is_command_allowed("echo $(rm -rf /)"));
}

#[test]
fn wildcard_unblocked_allows_dollar_brace() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("echo ${HOME}"));
    assert!(p.is_command_allowed("echo ${PATH}"));
}

#[test]
fn wildcard_unblocked_allows_process_substitution() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("diff <(ls dir1) <(ls dir2)"));
    assert!(p.is_command_allowed("tee >(grep error > errors.log)"));
}

#[test]
fn wildcard_unblocked_allows_pipes_and_chains() {
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("ps aux | grep python | wc -l"));
    assert!(p.is_command_allowed("echo hello && echo world"));
}

#[test]
fn wildcard_blocked_still_runs_shell_guard() {
    // allowed_commands=["*"] but block_high_risk_commands=true (default)
    // — the shell expansion guard must still fire.
    let p = SecurityPolicy {
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("echo `whoami`"));
    assert!(!p.is_command_allowed("echo $(cat /etc/passwd)"));
    assert!(!p.is_command_allowed("echo ${HOME}"));
    assert!(!p.is_command_allowed("diff <(ls dir1) <(ls dir2)"));
}

#[test]
fn specific_allowlist_still_runs_shell_guard() {
    // Non-wildcard allowlist — the guard must always run regardless
    // of block_high_risk_commands.
    let p = SecurityPolicy {
        allowed_commands: vec!["echo".into(), "ls".into(), "diff".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("echo `whoami`"));
    assert!(!p.is_command_allowed("echo $(cat /etc/passwd)"));
    assert!(!p.is_command_allowed("echo ${HOME}"));
    assert!(!p.is_command_allowed("diff <(ls dir1) <(ls dir2)"));
}

#[test]
fn specific_allowlist_with_block_true_still_runs_shell_guard() {
    let p = SecurityPolicy {
        allowed_commands: vec!["echo".into(), "ls".into()],
        block_high_risk_commands: true,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("echo `whoami`"));
    assert!(!p.is_command_allowed("echo $(rm -rf /)"));
    assert!(!p.is_command_allowed("echo ${HOME}"));
}

#[test]
fn wildcard_unblocked_readonly_still_blocked() {
    // Even with wildcard + unblocked, ReadOnly trumps everything.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        allowed_commands: vec!["*".into()],
        block_high_risk_commands: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("echo `whoami`"));
}

#[test]
fn per_sender_tracker_isolates_counts() {
    let t = PerSenderTracker::new();
    // sender A hits limit=2 on 3rd call
    assert!(t.record_within("chat_a", 2)); // count=1 ≤ 2 → ok
    assert!(t.record_within("chat_a", 2)); // count=2 ≤ 2 → ok
    assert!(!t.record_within("chat_a", 2)); // count=3 > 2 → blocked
    // sender B is unaffected — its bucket is empty
    assert!(t.record_within("chat_b", 2)); // count=1 ≤ 2 → ok
    assert!(t.record_within("chat_b", 2)); // count=2 ≤ 2 → ok
    assert!(!t.record_within("chat_b", 2)); // count=3 > 2 → blocked
}

#[test]
fn per_sender_tracker_global_key_fallback() {
    let t = PerSenderTracker::new();
    assert!(!t.is_exhausted(PerSenderTracker::GLOBAL_KEY, 1));
    t.record_within(PerSenderTracker::GLOBAL_KEY, u32::MAX);
    // after 1 action, count=1 ≥ 1 → exhausted at max=1
    assert!(t.is_exhausted(PerSenderTracker::GLOBAL_KEY, 1));
}

#[test]
fn per_sender_tracker_is_exhausted_reads_without_spurious_insert() {
    let t = PerSenderTracker::new();
    // Key "ghost" has never been recorded — should not be exhausted at max=1
    assert!(!t.is_exhausted("ghost", 1));
}

#[test]
fn attached_short_option_value_handles_multibyte_token() {
    // A multibyte char immediately after the dash must not panic on a
    // byte-index slice. Regression for a char-boundary abort.
    assert_eq!(
        attached_short_option_value("-é/etc/passwd"),
        Some("/etc/passwd")
    );
    assert_eq!(attached_short_option_value("-—"), None);
    assert_eq!(
        attached_short_option_value("-f/etc/passwd"),
        Some("/etc/passwd")
    );
    assert_eq!(attached_short_option_value("-f"), None);
    assert_eq!(attached_short_option_value("--long"), None);
}
