use zeroclaw_config::autonomy::McpDiscoveredToolPolicy;
use zeroclaw_config::schema::{Config, RiskProfileConfig};
use zeroclaw_policy::SecurityPolicy;

#[test]
fn omitted_profile_limits_match_runtime_policy_defaults() {
    let parsed: RiskProfileConfig = toml::from_str("").expect("an omitted profile uses defaults");
    let configured = RiskProfileConfig::default();
    let policy = SecurityPolicy::default();

    assert_eq!(parsed.allowed_commands, configured.allowed_commands);
    assert_eq!(parsed.forbidden_paths, configured.forbidden_paths);
    assert_eq!(policy.allowed_commands, configured.allowed_commands);
    assert_eq!(policy.forbidden_paths, configured.forbidden_paths);
}

#[test]
fn configured_empty_card_closes_permissive_profile_at_policy_boundary() {
    let root = tempfile::tempdir().expect("temporary install root");
    let mut config: Config = toml::from_str(
        r#"
schema_version = 3

[risk_profiles.operator]
allowed_tools = ["tachi_start"]
mcp_discovered_tool_policy = "auto_admit"

[cards.restricted]
risk_profile = "operator"

[agents.body]
card = "restricted"
"#,
    )
    .expect("a card-only agent and its named risk profile parse");
    config.config_path = root.path().join("config.toml");
    config.data_dir = root.path().join("data");

    let profile = config.risk_profiles.get("operator").expect("named profile");
    assert_eq!(profile.allowed_tools, Some(vec!["tachi_start".to_string()]));
    assert_eq!(
        profile.mcp_discovered_tool_policy,
        McpDiscoveredToolPolicy::AutoAdmit
    );

    let policy = SecurityPolicy::for_agent(&config, "body").expect("resolve configured card");

    assert_eq!(policy.risk_profile_name, "operator");
    assert_eq!(policy.allowed_tools, Some(Vec::new()));
    assert!(!policy.is_tool_allowed("tachi_start"));
    assert!(!policy.is_tool_allowed("fixture__launch"));
    assert_eq!(
        policy.mcp_discovered_tool_policy,
        McpDiscoveredToolPolicy::ExplicitOnly
    );
    assert!(
        !policy
            .mcp_discovered_tool_policy
            .admits_unlisted("fixture__launch")
    );
    assert_eq!(policy.workspace_dir, config.agent_workspace_dir("body"));
    assert!(policy.workspace_dir.is_dir());
}
