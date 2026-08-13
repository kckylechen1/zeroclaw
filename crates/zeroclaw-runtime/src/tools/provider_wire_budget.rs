//! Test-only provider-wire token budget for the Hyperion lean profile.
//!
//! Counts tokens the way a native OpenAI turn does: `build_iteration_tool_specs`
//! then `OpenAiModelProvider::chat_tools_wire` (the `NativeChatRequest.tools`
//! array). The gate is `ceil(tools_json.len() / 4)` on that whole array, not a
//! sum of per-tool ceils.

use super::all_tools_with_runtime;
use super::scoped::{ScopedAssembled, ScopedAssembly, ScopedToolRegistry};
use crate::platform::NativeRuntime;
use crate::security::SecurityPolicy;
use crate::skills::Skill;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use zeroclaw_config::schema::{Config, MemoryConfig};
use zeroclaw_memory::Memory;
use zeroclaw_providers::openai::OpenAiModelProvider;

const LEAN_PROFILE_TOML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/hyperion-lean-profile.toml"
));

/// Copied `SKILL.md` files from `.claude/skills/{zeroclaw,changelog-generation}/`.
/// This repo has no Hyperion trading skill bundle; these are contributor
/// instruction-only skills (no `SKILL.toml` tools), not a product bundle.
const COPIED_SKILL_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/lean-skill-bundle"
);

/// Phrase from the copied `zeroclaw/SKILL.md` body, not its description.
const SKILL_BODY_PHRASE: &str = "Adaptive Expertise";
const MEMORY_MARKER: &str = "HYPERION_LEAN_MEMORY_MD_MARKER";

/// Loose freeze. The gate is the worst-case lean assembly (copied skill
/// fixture + ExplicitOnly MCP + WeChat `inject_memory = false`).
const LEAN_PROVIDER_WIRE_TOKEN_CEILING: usize = 5_000;

const PARSE_HELPER_TABLES: &[&str] = &[
    "data_retention",
    "cloud_ops",
    "conversational_ai",
    "security",
    "security_ops",
];

#[derive(Debug, Clone)]
struct ToolWireRow {
    name: String,
    parameters_tokens: usize,
    native_tokens: usize,
}

#[derive(Debug)]
struct WireBudget {
    names: Vec<String>,
    rows: Vec<ToolWireRow>,
    parameters_tokens: usize,
    native_tools_tokens: usize,
    system_prompt: String,
    system_prompt_tokens: usize,
    whole_turn_tokens: usize,
    tool_search_present: bool,
}

fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

fn parse_fragment(raw: &str) -> Config {
    let mut merged = raw.trim().to_string();
    for table in PARSE_HELPER_TABLES {
        if merged.contains(&format!("[{table}]")) {
            continue;
        }
        merged.push_str("\n\n[");
        merged.push_str(table);
        merged.push(']');
    }
    merged.push('\n');
    toml::from_str(&merged).unwrap_or_else(|e| panic!("lean profile toml must parse: {e}"))
}

fn pin_install(config: &mut Config, tmp: &TempDir) {
    config.config_path = tmp.path().join("config.toml");
    config.data_dir = tmp.path().join("data");
    if let Some(agent) = config.agents.get_mut("hyperion") {
        agent.workspace.path = Some(tmp.path().join("workspace"));
    }
}

fn lean_config(tmp: &TempDir) -> Config {
    let mut config = parse_fragment(LEAN_PROFILE_TOML);
    pin_install(&mut config, tmp);
    config
}

fn copied_skill_fixture_dir() -> PathBuf {
    PathBuf::from(COPIED_SKILL_FIXTURE)
}

fn attach_copied_skill_fixture(config: &mut Config) {
    let dir = copied_skill_fixture_dir();
    assert!(
        dir.join("zeroclaw").join("SKILL.md").is_file(),
        "copied skill fixture missing: {}",
        dir.display()
    );
    config.skill_bundles.insert(
        "copied_skills".into(),
        zeroclaw_config::schema::SkillBundleConfig {
            directory: Some(dir.to_string_lossy().into_owned()),
            include: Vec::new(),
            exclude: Vec::new(),
        },
    );
    config
        .agents
        .get_mut("hyperion")
        .unwrap()
        .skill_bundles
        .push("copied_skills".into());
}

fn point_hapi_edge_at(config: &mut Config, url: String) {
    config.mcp.servers.retain(|s| s.name == "hapi-edge");
    config.mcp.servers[0].url = Some(url);
    config.mcp.servers[0].transport = zeroclaw_config::schema::McpTransport::Http;
    config.mcp_bundles.insert(
        "hyperion".into(),
        zeroclaw_config::schema::McpBundleConfig {
            servers: vec!["hapi-edge".into()],
            exclude: Vec::new(),
        },
    );
}

fn seed_personality(workspace: &Path, include_memory: bool) {
    std::fs::create_dir_all(workspace).unwrap();
    let files = [
        (
            "AGENTS.md",
            include_str!("../agent/personality_templates/AGENTS.md"),
        ),
        (
            "SOUL.md",
            include_str!("../agent/personality_templates/SOUL.md"),
        ),
        (
            "TOOLS.md",
            include_str!("../agent/personality_templates/TOOLS.md"),
        ),
        (
            "IDENTITY.md",
            include_str!("../agent/personality_templates/IDENTITY.md"),
        ),
        (
            "USER.md",
            include_str!("../agent/personality_templates/USER.md"),
        ),
    ];
    for (name, body) in files {
        std::fs::write(workspace.join(name), body).unwrap();
    }
    let memory = if include_memory {
        format!(
            "{}\n{MEMORY_MARKER}\n",
            include_str!("../agent/personality_templates/MEMORY.md")
        )
    } else {
        format!("{MEMORY_MARKER}\nnever inject this on the WeChat path\n")
    };
    std::fs::write(workspace.join("MEMORY.md"), memory).unwrap();
}

async fn mock_mcp_http_server(tools: &[(&str, &str)]) -> wiremock::MockServer {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method": "initialize"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Mcp-Session-Id", "s")
                .set_body_json(serde_json::json!({
                    "jsonrpc":"2.0","id":1,
                    "result":{"capabilities":{"tools":{}}}
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method":"notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let tool_json: Vec<Value> = tools
        .iter()
        .map(|(name, desc)| {
            serde_json::json!({
                "name": name,
                "description": desc,
                "inputSchema": {"type": "object"}
            })
        })
        .collect();
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method":"tools/list"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc":"2.0","id":2,"result":{"tools":tool_json}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"method":"resources/list"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc":"2.0","id":3,"result":{"resources":[]}
        })))
        .mount(&server)
        .await;
    server
}

struct TurnRequest<'a> {
    config: &'a Config,
    agent_alias: &'a str,
    skills: &'a [Skill],
    connect_mcp: bool,
    inject_memory: bool,
    workspace: &'a Path,
}

async fn assemble_turn(req: TurnRequest<'_>) -> (ScopedAssembled, WireBudget) {
    let workspace = req.workspace;
    std::fs::create_dir_all(workspace).unwrap();
    let mem_cfg = MemoryConfig {
        backend: "markdown".into(),
        ..MemoryConfig::default()
    };
    let mem: Arc<dyn Memory> =
        Arc::from(zeroclaw_memory::create_memory(&mem_cfg, workspace, None).unwrap());
    let runtime: Arc<dyn crate::platform::RuntimeAdapter> = Arc::new(NativeRuntime::new());
    let security = if req.config.agents.contains_key(req.agent_alias) {
        Arc::new(
            SecurityPolicy::for_agent(req.config, req.agent_alias)
                .expect("lean agent must resolve a risk profile"),
        )
    } else {
        Arc::new(SecurityPolicy {
            workspace_dir: workspace.to_path_buf(),
            ..SecurityPolicy::default()
        })
    };
    let risk_profile = req
        .config
        .risk_profile_for_agent(req.agent_alias)
        .cloned()
        .unwrap_or_default();
    let built = all_tools_with_runtime(
        Arc::new(req.config.clone()),
        &security,
        &risk_profile,
        req.agent_alias,
        Arc::clone(&runtime),
        mem,
        None,
        None,
        &req.config.browser,
        &req.config.http_request,
        &req.config.web_fetch,
        workspace,
        &req.config.agents,
        None,
        req.config,
        None,
        false,
        None,
        None,
        None,
        None,
    );
    let assembled = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        ScopedToolRegistry::assemble(ScopedAssembly {
            config: req.config,
            agent_alias: req.agent_alias,
            security: &security,
            built,
            skills: req.skills,
            runtime,
            caller_allowed: None,
            connect_mcp: req.connect_mcp,
            connect_peripherals: false,
            exclude_memory: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: false,
            mcp_registry: None,
        }),
    )
    .await
    .expect("assemble must not hang");

    let provider = OpenAiModelProvider::builder("wire-budget").build();
    let iteration = crate::agent::turn::build_iteration_tool_specs(
        &provider,
        &assembled.registry,
        &[],
        assembled.activated_handle.as_ref(),
    )
    .expect("iteration tool specs");
    let tools_json = OpenAiModelProvider::chat_tools_wire(&iteration.tool_specs)
        .expect("native tools array must serialize");
    let native_tools_tokens = estimate_tokens(&tools_json);
    let params_json = serde_json::to_string(
        &iteration
            .tool_specs
            .iter()
            .map(|spec| spec.parameters.as_ref())
            .collect::<Vec<_>>(),
    )
    .expect("parameter schemas must serialize");
    let parameters_tokens = estimate_tokens(&params_json);

    let parsed: Vec<Value> = serde_json::from_str(&tools_json).unwrap_or_default();
    let mut rows: Vec<ToolWireRow> = parsed
        .iter()
        .map(|item| {
            let name = item
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let parameters = item
                .pointer("/function/parameters")
                .cloned()
                .unwrap_or(Value::Null);
            ToolWireRow {
                name,
                parameters_tokens: estimate_tokens(&parameters.to_string()),
                native_tokens: estimate_tokens(&item.to_string()),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.native_tokens
            .cmp(&a.native_tokens)
            .then(a.name.cmp(&b.name))
    });

    let names: Vec<String> = assembled
        .registry
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let tool_search_present = names.iter().any(|n| n == "tool_search");
    let tool_pairs: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "")).collect();
    let skills_mode = req.config.effective_skills_prompt_mode(req.agent_alias);
    let mut system_prompt = crate::agent::system_prompt::build_system_prompt_with_persona(
        workspace,
        "test-model",
        &tool_pairs,
        req.skills,
        None,
        Some(6000),
        Some(&risk_profile),
        true,
        skills_mode,
        true,
        0,
        req.inject_memory,
        false,
        None,
    );
    let mcp_section = assembled.combined_mcp_prompt_section();
    if !mcp_section.is_empty() {
        system_prompt.push('\n');
        system_prompt.push_str(&mcp_section);
    }
    let system_prompt_tokens = estimate_tokens(&system_prompt);
    let whole_turn_tokens = system_prompt_tokens + native_tools_tokens;
    (
        assembled,
        WireBudget {
            names,
            rows,
            parameters_tokens,
            native_tools_tokens,
            system_prompt,
            system_prompt_tokens,
            whole_turn_tokens,
            tool_search_present,
        },
    )
}

fn print_budget(label: &str, budget: &WireBudget) {
    eprintln!("=== provider-wire budget: {label} ===");
    eprintln!("{:<28} {:>10} {:>10}", "tool", "params_tok", "native_tok");
    for row in &budget.rows {
        eprintln!(
            "{:<28} {:>10} {:>10}",
            row.name, row.parameters_tokens, row.native_tokens
        );
    }
    eprintln!(
        "tools={} parameters_tok={} native_tools_tok={} system_prompt_tok={} whole_turn_tok={}",
        budget.names.len(),
        budget.parameters_tokens,
        budget.native_tools_tokens,
        budget.system_prompt_tokens,
        budget.whole_turn_tokens
    );
}

#[test]
fn lean_profile_toml_parses_as_config() {
    let tmp = TempDir::new().unwrap();
    let config = lean_config(&tmp);
    assert!(
        config.mcp.deferred_loading,
        "deferred_loading is global [mcp]"
    );
    let risk = config
        .risk_profiles
        .get("hyperion_lean")
        .expect("risk profile");
    let allowed = risk
        .allowed_tools
        .as_ref()
        .expect("allowed_tools must be set");
    assert!(
        allowed.iter().any(|n| n == "hapi-edge__snapshot"),
        "ExplicitOnly requires named facades: {allowed:?}"
    );
    assert!(
        allowed.iter().any(|n| n == "hapi-memory__hapi_memory"),
        "memory facade must be named: {allowed:?}"
    );
    assert!(
        !allowed.iter().any(|n| n == "hapi-memory__hapi_save"),
        "withdrawn independent memory tools must stay off the lean list"
    );
    let agent = config.agents.get("hyperion").expect("agent alias");
    assert_eq!(agent.risk_profile.as_str(), "hyperion_lean");
    assert_eq!(agent.runtime_profile.as_str(), "hyperion_lean");
    assert_eq!(
        config.effective_skills_prompt_mode("hyperion"),
        zeroclaw_config::schema::SkillsPromptInjectionMode::Compact
    );
}

#[tokio::test]
async fn provider_wire_budget_default_and_lean_no_skills() {
    // Default counts are informational. Upstream adding a built-in must not
    // fail this test; the freeze lives on the worst-case lean assembly.
    let default_tmp = TempDir::new().unwrap();
    seed_personality(default_tmp.path(), true);
    let default_config = Config {
        config_path: default_tmp.path().join("config.toml"),
        data_dir: default_tmp.path().join("data"),
        ..Config::default()
    };
    let (_, default_budget) = assemble_turn(TurnRequest {
        config: &default_config,
        agent_alias: "default",
        skills: &[],
        connect_mcp: false,
        inject_memory: true,
        workspace: default_tmp.path(),
    })
    .await;
    print_budget("default (informational)", &default_budget);

    let lean_tmp = TempDir::new().unwrap();
    let lean = lean_config(&lean_tmp);
    let workspace = lean.agent_workspace_dir("hyperion");
    seed_personality(&workspace, true);
    let (_, lean_budget) = assemble_turn(TurnRequest {
        config: &lean,
        agent_alias: "hyperion",
        skills: &[],
        connect_mcp: false,
        inject_memory: false,
        workspace: &workspace,
    })
    .await;
    print_budget("hyperion_lean", &lean_budget);

    assert!(
        default_budget.names.len() >= 40,
        "production path is all_tools_with_runtime, not default_tools(6); got {:?}",
        default_budget.names
    );
    assert!(
        default_budget.names.iter().any(|n| n == "cron_add"),
        "default registry must still carry the fat built-ins: {:?}",
        default_budget.names
    );
    assert!(
        !lean_budget.names.iter().any(|n| n == "cron_add"),
        "lean allow-list must drop cron_add: {:?}",
        lean_budget.names
    );
    for required in [
        "shell",
        "file_read",
        "file_write",
        "file_edit",
        "glob_search",
        "content_search",
        "memory_recall",
        "memory_store",
        "web_search_tool",
        "web_fetch",
        "read_skill",
    ] {
        assert!(
            lean_budget.names.iter().any(|n| n == required),
            "lean registry missing {required}: {:?}",
            lean_budget.names
        );
    }
    assert!(
        lean_budget.native_tools_tokens < default_budget.native_tools_tokens,
        "lean native tools {} should be below default {}",
        lean_budget.native_tools_tokens,
        default_budget.native_tools_tokens
    );
}

#[tokio::test]
async fn lean_compact_skills_omit_skill_bodies() {
    let tmp = TempDir::new().unwrap();
    let mut config = lean_config(&tmp);
    attach_copied_skill_fixture(&mut config);
    let workspace = config.agent_workspace_dir("hyperion");
    seed_personality(&workspace, false);
    let skills = crate::skills::load_skills_for_agent_from_config(&config, "hyperion");
    assert!(
        skills.iter().any(|s| s.name == "zeroclaw"),
        "expected copied SKILL.md fixture to load zeroclaw, got {:?}",
        skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );
    let (_, budget) = assemble_turn(TurnRequest {
        config: &config,
        agent_alias: "hyperion",
        skills: &skills,
        connect_mcp: false,
        inject_memory: false,
        workspace: &workspace,
    })
    .await;
    print_budget("hyperion_lean+copied_skills", &budget);
    assert!(
        budget.system_prompt.contains("zeroclaw"),
        "compact mode still lists skill name"
    );
    assert!(
        !budget.system_prompt.contains(SKILL_BODY_PHRASE),
        "compact mode must not inline SKILL.md body"
    );
}

#[tokio::test]
async fn lean_explicit_only_mcp_filters_unlisted_and_needs_named_facades() {
    let server = mock_mcp_http_server(&[
        ("snapshot", "read a quote"),
        ("secret_write", "must never reach the model"),
    ])
    .await;

    let listed_tmp = TempDir::new().unwrap();
    let mut listed = lean_config(&listed_tmp);
    point_hapi_edge_at(&mut listed, server.uri());
    let workspace = listed.agent_workspace_dir("hyperion");
    seed_personality(&workspace, false);
    let (assembled, budget) = assemble_turn(TurnRequest {
        config: &listed,
        agent_alias: "hyperion",
        skills: &[],
        connect_mcp: true,
        inject_memory: false,
        workspace: &workspace,
    })
    .await;
    print_budget("hyperion_lean+explicit_mcp", &budget);
    let deferred = assembled.combined_mcp_prompt_section();
    assert!(
        budget.tool_search_present,
        "named facades must keep tool_search registered: {:?}",
        budget.names
    );
    assert!(
        deferred.contains("hapi-edge__snapshot"),
        "listed facade must appear in the deferred section: {deferred}"
    );
    assert!(
        !deferred.contains("secret_write"),
        "unlisted MCP tool must not appear: {deferred}"
    );
    assert!(
        !budget.names.iter().any(|n| n.contains("secret_write")),
        "unlisted MCP tool must not enter the registry: {:?}",
        budget.names
    );

    let empty_tmp = TempDir::new().unwrap();
    let mut empty = listed.clone();
    pin_install(&mut empty, &empty_tmp);
    let empty_workspace = empty.agent_workspace_dir("hyperion");
    seed_personality(&empty_workspace, false);
    if let Some(risk) = empty.risk_profiles.get_mut("hyperion_lean") {
        risk.allowed_tools = Some(vec![
            "shell".into(),
            "file_read".into(),
            "file_write".into(),
            "file_edit".into(),
            "glob_search".into(),
            "content_search".into(),
            "memory_recall".into(),
            "memory_store".into(),
            "web_search_tool".into(),
            "web_fetch".into(),
            "read_skill".into(),
        ]);
    }
    let (_, empty_budget) = assemble_turn(TurnRequest {
        config: &empty,
        agent_alias: "hyperion",
        skills: &[],
        connect_mcp: true,
        inject_memory: false,
        workspace: &empty_workspace,
    })
    .await;
    print_budget("hyperion_lean+mcp_no_facades", &empty_budget);
    assert!(
        !empty_budget.tool_search_present,
        "ExplicitOnly with no named facades must not register tool_search: {:?}",
        empty_budget.names
    );
}

#[tokio::test]
async fn lean_wechat_path_skips_memory_md() {
    let tmp = TempDir::new().unwrap();
    let config = lean_config(&tmp);
    let workspace = config.agent_workspace_dir("hyperion");
    seed_personality(&workspace, true);
    let (_, wechat) = assemble_turn(TurnRequest {
        config: &config,
        agent_alias: "hyperion",
        skills: &[],
        connect_mcp: false,
        inject_memory: false,
        workspace: &workspace,
    })
    .await;
    let (_, interactive) = assemble_turn(TurnRequest {
        config: &config,
        agent_alias: "hyperion",
        skills: &[],
        connect_mcp: false,
        inject_memory: true,
        workspace: &workspace,
    })
    .await;
    print_budget("hyperion_lean wechat inject_memory=false", &wechat);
    print_budget("hyperion_lean interactive inject_memory=true", &interactive);
    assert!(
        !wechat.system_prompt.contains(MEMORY_MARKER),
        "WeChat process_message path must omit MEMORY.md"
    );
    assert!(
        interactive.system_prompt.contains(MEMORY_MARKER),
        "interactive inject_memory=true must include MEMORY.md"
    );
}

#[tokio::test]
async fn lean_worst_case_skills_and_mcp_under_ceiling() {
    let server = mock_mcp_http_server(&[
        ("snapshot", "read a quote"),
        ("secret_write", "must never reach the model"),
    ])
    .await;
    let tmp = TempDir::new().unwrap();
    let mut config = lean_config(&tmp);
    attach_copied_skill_fixture(&mut config);
    point_hapi_edge_at(&mut config, server.uri());
    let workspace = config.agent_workspace_dir("hyperion");
    seed_personality(&workspace, true);
    let skills = crate::skills::load_skills_for_agent_from_config(&config, "hyperion");
    assert!(
        !skills.is_empty(),
        "copied SKILL.md fixture must load at least one skill"
    );
    let (assembled, budget) = assemble_turn(TurnRequest {
        config: &config,
        agent_alias: "hyperion",
        skills: &skills,
        connect_mcp: true,
        inject_memory: false,
        workspace: &workspace,
    })
    .await;
    print_budget("hyperion_lean+copied_skills+explicit_mcp (gate)", &budget);
    eprintln!(
        "loaded_skills={} registry_tools={:?} skill_tools={:?}",
        skills.len(),
        budget.names,
        skills
            .iter()
            .flat_map(|s| s.tools.iter().map(|t| format!("{}__{}", s.name, t.name)))
            .collect::<Vec<_>>()
    );
    let deferred = assembled.combined_mcp_prompt_section();
    assert!(
        budget.tool_search_present,
        "worst-case lean must keep tool_search: {:?}",
        budget.names
    );
    assert!(
        !deferred.contains("secret_write"),
        "unlisted MCP tool must not appear: {deferred}"
    );
    assert!(
        !budget.system_prompt.contains(SKILL_BODY_PHRASE),
        "compact mode must not inline SKILL.md body"
    );
    assert!(
        budget.whole_turn_tokens <= LEAN_PROVIDER_WIRE_TOKEN_CEILING,
        "worst-case lean whole-turn {} exceeded freeze {} (system {} + tools[] {} ; registry {:?})",
        budget.whole_turn_tokens,
        LEAN_PROVIDER_WIRE_TOKEN_CEILING,
        budget.system_prompt_tokens,
        budget.native_tools_tokens,
        budget.names
    );
}
