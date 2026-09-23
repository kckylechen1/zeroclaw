//! Governed Soul projection (ADR-015 §2, §6).
//!
//! Renders the agent's persona section from three sources, in order:
//!
//! 1. `## Identity`: the owner-governed Identity layer of the Soul profile
//!    store, plus a fixed honesty line (ADR-015 §5);
//! 2. `## Principles`: the owner-governed Principles layer;
//! 3. `## Voice`: the configured persona dials ([`PersonaKnobs`]).
//!
//! Only owner-submitted text, seeded defaults, and repository-owned strings
//! can render; there is no path from model output into this section. Each
//! layer is byte-bounded and truncated by whole lines only.
//!
//! The same profile and config always produce the same bytes, whatever the
//! model, provider, or channel carrying the turn.
//!
//! Legacy `SOUL.md` / `IDENTITY.md` workspace files keep being injected until
//! the owner writes an Identity revision; after that they are suppressed so
//! there is one persona source.
//!
//! [`PersonaKnobs`]: zeroclaw_config::persona::PersonaKnobs

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use zeroclaw_config::schema::Config;
use zeroclaw_memory::companion::{SoulProfile, SoulProfileStore};

/// Byte ceiling of the rendered `## Identity` section.
pub const IDENTITY_SECTION_MAX_BYTES: usize = 512;
/// Byte ceiling of the rendered `## Principles` section.
pub const PRINCIPLES_SECTION_MAX_BYTES: usize = 2048;

/// Fixed honesty floor rendered with every governed identity.
pub const IDENTITY_HONESTY_LINE: &str = "If someone sincerely asks whether they are talking to an AI, \
     or which model is answering, tell them the truth.";

/// Legacy persona files that the governed Identity layer replaces.
pub const LEGACY_PERSONA_FILES: &[&str] = &["SOUL.md", "IDENTITY.md"];

/// Whether the prompt builders still inject the legacy persona files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LegacyPersonaFiles {
    /// Inject `SOUL.md` / `IDENTITY.md` as before (owner has not migrated).
    #[default]
    Inject,
    /// Skip them: the owner-authored Identity layer is the persona source.
    Suppress,
}

impl LegacyPersonaFiles {
    /// Whether `filename` should be left out of the prompt.
    #[must_use]
    pub fn skips(self, filename: &str) -> bool {
        self == Self::Suppress && LEGACY_PERSONA_FILES.contains(&filename)
    }
}

/// Everything a prompt builder needs from the persona.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonaProjection {
    /// Rendered Identity + Principles + Voice, or `None` when all are empty.
    pub section: Option<String>,
    pub legacy_files: LegacyPersonaFiles,
}

/// Resolve the persona projection for one agent.
///
/// Seeds missing Soul layers on first use. Store failures never fail the
/// turn: they log one WARN and fall back to the configured Voice dials plus
/// the legacy files.
#[must_use]
pub fn persona_projection(config: &Config, agent_alias: &str) -> PersonaProjection {
    let voice = config
        .persona_for_agent(agent_alias)
        .and_then(zeroclaw_config::persona::PersonaKnobs::to_prompt_section);

    // The prompt path never creates the data directory: an install that has
    // not been set up (or a test using a default config) has no Soul yet.
    let profile = if config.data_dir.is_dir() {
        SoulProfileStore::shared(&config.data_dir)
            .and_then(|store| {
                store.ensure_seeded(
                    agent_alias,
                    zeroclaw_memory::companion::seed_name_for_agent(agent_alias),
                    now_unix(),
                )
            })
            .map(Some)
    } else {
        Ok(None)
    };
    let profile = match profile {
        Ok(profile) => profile,
        Err(err) => {
            warn_once(
                &format!("store:{agent_alias}"),
                "agent.soul_profile_unavailable",
                &format!(
                    "Soul profile for agent {agent_alias} is unavailable ({err}); \
                     using configured voice and legacy persona files only"
                ),
            );
            None
        }
    };

    let legacy_files = if profile
        .as_ref()
        .is_some_and(SoulProfile::identity_is_owner_authored)
    {
        LegacyPersonaFiles::Suppress
    } else {
        warn_if_legacy_files_present(config, agent_alias);
        LegacyPersonaFiles::Inject
    };

    let mut parts: Vec<String> = Vec::new();
    if let Some(profile) = &profile {
        parts.extend(render_identity(profile));
        parts.extend(render_principles(profile));
    }
    parts.extend(voice);
    let section = (!parts.is_empty()).then(|| {
        parts
            .iter()
            .map(|part| part.trim_end())
            .collect::<Vec<_>>()
            .join("\n\n")
            + "\n"
    });
    PersonaProjection {
        section,
        legacy_files,
    }
}

/// Render `## Identity`, or `None` when the layer is absent.
#[must_use]
pub fn render_identity(profile: &SoulProfile) -> Option<String> {
    let identity = &profile.identity.as_ref()?.value;
    let mut first = format!("You are {}.", identity.name);
    if let Some(description) = &identity.self_description {
        first.push(' ');
        first.push_str(description);
    }
    let mut details = Vec::new();
    if let Some(language) = &identity.primary_language {
        details.push(format!("Primary language: {language}."));
    }
    if let Some(pronouns) = &identity.pronouns {
        details.push(format!("Pronouns: {pronouns}."));
    }
    // Required lines always render (their maximum size fits the ceiling by
    // construction); optional lines are added while they fit.
    let required = [first, IDENTITY_HONESTY_LINE.to_string()];
    let optional = (!details.is_empty()).then(|| details.join(" "));
    let mut out = String::from("## Identity\n\n");
    out.push_str(&required[0]);
    out.push('\n');
    if let Some(line) = optional
        // +2: the newline after this line and after the honesty line.
        && out.len() + line.len() + required[1].len() + 2 <= IDENTITY_SECTION_MAX_BYTES
    {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&required[1]);
    out.push('\n');
    Some(out)
}

/// Render `## Principles`, or `None` when the layer is absent or empty.
#[must_use]
pub fn render_principles(profile: &SoulProfile) -> Option<String> {
    let items = &profile.principles.as_ref()?.value.items;
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("## Principles\n\n");
    for (index, item) in items.iter().enumerate() {
        let line = format!("{}. {item}\n", index + 1);
        if out.len() + line.len() > PRINCIPLES_SECTION_MAX_BYTES {
            let marker = format!("(+{} principles elided)\n", items.len() - index);
            if out.len() + marker.len() <= PRINCIPLES_SECTION_MAX_BYTES {
                out.push_str(&marker);
            }
            break;
        }
        out.push_str(&line);
    }
    Some(out)
}

fn warn_if_legacy_files_present(config: &Config, agent_alias: &str) {
    let workspace = config.agent_workspace_dir(agent_alias);
    if LEGACY_PERSONA_FILES
        .iter()
        .any(|name| workspace.join(name).is_file())
    {
        warn_once(
            &format!("legacy:{agent_alias}"),
            "agent.legacy_persona_files_injected",
            &format!(
                "Agent {agent_alias} still injects legacy SOUL.md / IDENTITY.md from {}. \
                 Move what you want to keep into the governed Soul \
                 (PUT /api/soul/identity and /api/soul/principles); \
                 the legacy files stop being injected after that.",
                display(&workspace)
            ),
        );
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn warn_once(key: &str, error_key: &str, message: &str) {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
    let first = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.to_string());
    if first {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({ "error_key": error_key })),
            message
        );
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_memory::companion::{
        DEFAULT_PRINCIPLES, SOUL_MAX_PRINCIPLES, SOUL_NAME_MAX_BYTES, SOUL_PRINCIPLE_MAX_BYTES,
        SOUL_SELF_DESCRIPTION_MAX_BYTES, SOUL_SHORT_FIELD_MAX_BYTES, SoulIdentity, SoulPrinciples,
    };

    fn config_in(dir: &Path) -> Config {
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        Config {
            data_dir,
            ..Config::default()
        }
    }

    #[test]
    fn fresh_agent_gets_seeded_identity_and_principles_and_keeps_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let projection = persona_projection(&config, "nova");
        let section = projection.section.unwrap();
        assert!(
            section.starts_with("## Identity\n\nYou are nova.\n"),
            "{section}"
        );
        assert!(section.contains(IDENTITY_HONESTY_LINE));
        assert!(section.contains("## Principles\n\n1. "));
        for principle in DEFAULT_PRINCIPLES {
            assert!(section.contains(principle), "{principle}");
        }
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Inject);
    }

    #[test]
    fn owner_identity_suppresses_legacy_files_and_renders_owner_text() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let store = SoulProfileStore::shared(&config.data_dir).unwrap();
        store.ensure_seeded("nova", "nova", 1).unwrap();
        store
            .set_identity(
                "nova",
                SoulIdentity {
                    name: "Nova".into(),
                    self_description: Some("Kyle's personal agent.".into()),
                    primary_language: Some("zh-CN".into()),
                    pronouns: None,
                },
                1,
                2,
            )
            .unwrap();
        store
            .set_principles(
                "nova",
                SoulPrinciples {
                    items: vec!["Protect the owner's time.".into()],
                },
                1,
                2,
            )
            .unwrap();
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Suppress);
        let section = projection.section.unwrap();
        assert!(section.contains("You are Nova. Kyle's personal agent.\n"));
        assert!(section.contains("Primary language: zh-CN.\n"));
        assert!(section.contains("1. Protect the owner's time.\n"));
        assert!(!section.contains(DEFAULT_PRINCIPLES[0]));
    }

    #[test]
    fn projection_is_byte_identical_for_the_same_profile() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let first = persona_projection(&config, "nova");
        let second = persona_projection(&config, "nova");
        assert_eq!(first, second);
    }

    #[test]
    fn unavailable_store_falls_back_to_voice_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        // A directory whose store file cannot be opened as a database.
        let config = config_in(dir.path());
        std::fs::create_dir_all(config.data_dir.join("soul.db")).unwrap();
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Inject);
        assert!(
            projection
                .section
                .as_deref()
                .is_none_or(|s| !s.contains("## Identity"))
        );
    }

    #[test]
    fn worst_case_identity_and_principles_stay_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let store = SoulProfileStore::open(dir.path()).unwrap();
        store
            .set_identity(
                "a",
                SoulIdentity {
                    name: "n".repeat(SOUL_NAME_MAX_BYTES),
                    self_description: Some("d".repeat(SOUL_SELF_DESCRIPTION_MAX_BYTES)),
                    primary_language: Some("l".repeat(SOUL_SHORT_FIELD_MAX_BYTES)),
                    pronouns: Some("p".repeat(SOUL_SHORT_FIELD_MAX_BYTES)),
                },
                0,
                1,
            )
            .unwrap();
        store
            .set_principles(
                "a",
                SoulPrinciples {
                    items: vec!["x".repeat(SOUL_PRINCIPLE_MAX_BYTES); SOUL_MAX_PRINCIPLES],
                },
                0,
                1,
            )
            .unwrap();
        let profile = store.profile("a").unwrap();
        let identity = render_identity(&profile).unwrap();
        assert!(
            identity.len() <= IDENTITY_SECTION_MAX_BYTES,
            "{}",
            identity.len()
        );
        assert!(identity.contains(IDENTITY_HONESTY_LINE));
        let principles = render_principles(&profile).unwrap();
        assert!(principles.len() <= PRINCIPLES_SECTION_MAX_BYTES);
        assert_eq!(
            principles.lines().filter(|l| l.contains(". x")).count(),
            SOUL_MAX_PRINCIPLES
        );
    }

    #[test]
    fn missing_data_dir_is_not_created_by_the_prompt_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().join("absent"),
            ..Config::default()
        };
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Inject);
        assert!(projection.section.is_none());
        assert!(!config.data_dir.exists());
    }

    #[test]
    fn legacy_suppression_only_skips_soul_and_identity() {
        let suppress = LegacyPersonaFiles::Suppress;
        assert!(suppress.skips("SOUL.md") && suppress.skips("IDENTITY.md"));
        for kept in ["USER.md", "AGENTS.md", "TOOLS.md", "MEMORY.md"] {
            assert!(!suppress.skips(kept), "{kept}");
        }
        assert!(!LegacyPersonaFiles::Inject.skips("SOUL.md"));
    }
}
