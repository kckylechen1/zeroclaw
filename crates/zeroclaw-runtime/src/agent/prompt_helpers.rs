//! Turn prompt assembly helpers: system prompt and export scrubbing.
//!
//! Extracted from `loop_.rs` so prompt/export utilities are not interleaved
//! with the interactive `run` / `process_message` entry points.

use crate::tools::Tool;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use zeroclaw_providers::ModelProvider;

use super::text_tool_prompt::{apply_text_tool_prompt_policy, build_tool_instructions_for_names};
use super::turn::scrub_credentials;

pub fn native_tool_specs_present_for_turn(
    model_provider: &dyn ModelProvider,
    tools_registry: &[Box<dyn Tool>],
    excluded_tools: &[String],
    activated_tools: Option<&Arc<Mutex<crate::tools::ActivatedToolSet>>>,
) -> Result<bool> {
    if !model_provider.supports_native_tools() {
        return Ok(false);
    }

    // Name-only presence check mirroring `build_iteration_tool_specs`'s
    // filtering, without assembling any specs tools are present if
    // the registry or the activated deferred set has a non-excluded name.
    let is_excluded = |name: &str| excluded_tools.iter().any(|ex| ex == name);
    if tools_registry.iter().any(|tool| !is_excluded(tool.name())) {
        return Ok(true);
    }
    let Some(at) = activated_tools else {
        return Ok(false);
    };
    let activated = match at.lock() {
        Ok(guard) => guard,
        // Same recovery as build_iteration_tool_specs: a poisoned lock is
        // still safe for a read-only name scan.
        Err(poisoned) => poisoned.into_inner(),
    };
    Ok(activated.tool_names().iter().any(|name| !is_excluded(name)))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_system_prompt_for_turn(
    agent_workspace: &std::path::Path,
    model_name: &str,
    tool_descs: &[(&str, &str)],
    deferred_section: &str,
    skills: &[crate::skills::Skill],
    identity_config: Option<&zeroclaw_config::schema::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    risk_profile: &zeroclaw_config::schema::RiskProfileConfig,
    model_provider: &dyn ModelProvider,
    tools_registry: &[Box<dyn Tool>],
    excluded_tools: &[String],
    activated_tools: Option<&Arc<Mutex<crate::tools::ActivatedToolSet>>>,
    strict_tool_parsing: bool,
    skills_prompt_mode: zeroclaw_config::schema::SkillsPromptInjectionMode,
    compact_context: bool,
    max_system_prompt_chars: usize,
    inject_memory: bool,
    show_tool_calls: bool,
    // Rendered `## Voice` section for this agent's persona, or `None`. See
    // `system_prompt::build_system_prompt_with_persona` for placement rules.
    persona_section: Option<&str>,
    // Whether legacy `SOUL.md` / `IDENTITY.md` still inject (ADR-015 §2).
    legacy_persona_files: crate::agent::persona_projection::LegacyPersonaFiles,
    thinking_prefix: Option<&str>,
) -> Result<String> {
    let native_tools = model_provider.supports_native_tools();
    let native_tool_specs_present = native_tool_specs_present_for_turn(
        model_provider,
        tools_registry,
        excluded_tools,
        activated_tools,
    )?;
    let excluded_tool_names: HashSet<&str> = excluded_tools.iter().map(String::as_str).collect();
    let effective_tool_names: HashSet<&str> = tools_registry
        .iter()
        .map(|tool| tool.name())
        .filter(|name| !excluded_tool_names.contains(*name))
        .collect();
    let mut turn_tool_descs = tool_descs.to_vec();
    turn_tool_descs.retain(|(name, _)| effective_tool_names.contains(name));
    let mut turn_deferred_section = deferred_section.to_string();
    let expose_text_tool_protocol = apply_text_tool_prompt_policy(
        native_tools,
        strict_tool_parsing,
        &mut turn_tool_descs,
        &mut turn_deferred_section,
    );
    let mut system_prompt = crate::agent::system_prompt::build_system_prompt_with_persona(
        agent_workspace,
        model_name,
        &turn_tool_descs,
        skills,
        identity_config,
        bootstrap_max_chars,
        Some(risk_profile),
        native_tool_specs_present,
        skills_prompt_mode,
        compact_context,
        max_system_prompt_chars,
        inject_memory,
        show_tool_calls,
        persona_section,
        legacy_persona_files,
    );

    if expose_text_tool_protocol {
        system_prompt.push_str(&build_tool_instructions_for_names(
            tools_registry,
            &effective_tool_names,
        ));
    }
    if !turn_deferred_section.is_empty() {
        system_prompt.push('\n');
        system_prompt.push_str(&turn_deferred_section);
    }
    if let Some(prefix) = thinking_prefix {
        system_prompt = format!("{prefix}\n\n{system_prompt}");
    }

    Ok(system_prompt)
}

pub fn make_query_summary(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    Some(truncate_with_ellipsis(&scrub_credentials(raw), 200))
}

#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) fn tools_to_openai_format(tools_registry: &[Box<dyn Tool>]) -> Vec<serde_json::Value> {
    tools_registry
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters": tool.parameters_schema()
                }
            })
        })
        .collect()
}

pub(crate) fn autosave_memory_key(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}
