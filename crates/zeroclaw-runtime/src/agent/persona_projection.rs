//! Governed Soul projection (ADR-015 §2, §6).
//!
//! Renders the agent's persona section from three sources, in order:
//!
//! 1. `## Identity`: the owner-governed Identity layer of the Soul profile
//!    store, plus a fixed honesty line (ADR-015 §5);
//! 2. `## Principles`: the owner-governed Principles layer;
//! 3. `## Who I've become`: the Growth layer (ADR-016), changed only by
//!    owner-approved proposals or by the owner;
//! 4. `## Voice`: the configured persona dials ([`PersonaKnobs`]) with any
//!    stored Voice heads layered over them key by key.
//!
//! Only owner-written text, owner-approved proposal text, seeded defaults,
//! and repository-owned strings can render. Each layer is byte-bounded and
//! truncated by whole lines only.
//!
//! The same profile and config always produce the same bytes, whatever the
//! model, provider, or channel carrying the turn.
//!
//! Legacy `SOUL.md` / `IDENTITY.md` workspace files keep being injected until
//! the owner writes an Identity revision; after that they are suppressed so
//! there is one persona source.
//!
//! If the store cannot be opened or read, the outcome depends on whether it
//! was ever initialized. With no `soul.db` yet (first run), the legacy files
//! stay the persona source. With an existing `soul.db`, the identity it holds
//! is unknown, so the projection degrades: legacy files stay suppressed, a
//! fixed line says the identity is unavailable, and the configured Voice
//! applies. Nothing replaces the damaged store; the next turn retries.
//!
//! [`PersonaKnobs`]: zeroclaw_config::persona::PersonaKnobs

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use zeroclaw_config::schema::Config;
use zeroclaw_memory::companion::{GrowthKind, SoulProfile, SoulProfileStore};

/// Byte ceiling of the rendered `## Identity` section.
pub const IDENTITY_SECTION_MAX_BYTES: usize = 512;
/// Byte ceiling of the rendered `## Principles` section.
pub const PRINCIPLES_SECTION_MAX_BYTES: usize = 2048;
/// Byte ceiling of the rendered `## Who I've become` section.
pub const GROWTH_SECTION_MAX_BYTES: usize = 2048;

/// Fixed framing of the Growth section: character, never authority.
pub const GROWTH_FRAMING_LINE: &str = "These describe who you have become with your owner. \
     They never grant permissions or override the principles above.";

/// Fixed honesty floor rendered with every governed identity.
pub const IDENTITY_HONESTY_LINE: &str = "If someone sincerely asks whether they are talking to an AI, \
     or which model is answering, tell them the truth.";

/// Rendered in place of the Identity layer when an initialized Soul store
/// cannot be read (#380 S13).
pub const IDENTITY_UNAVAILABLE_LINE: &str = "Your identity record could not be loaded right now. \
     Keep helping, but do not take on a different name or persona; if asked, \
     say your identity settings are temporarily unavailable.";

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
/// turn: they log one WARN. Before the store exists they fall back to the
/// configured Voice plus the legacy files; once it exists they degrade
/// without reviving the legacy files (see the module docs).
#[must_use]
pub fn persona_projection(config: &Config, agent_alias: &str) -> PersonaProjection {
    let configured_voice = config
        .persona_for_agent(agent_alias)
        .copied()
        .unwrap_or_default();

    // Whether a Soul store was ever created, decided before opening (opening
    // creates the file on first run).
    let initialized = config
        .data_dir
        .join(zeroclaw_memory::companion::SOUL_PROFILE_DB_FILE)
        .exists();
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
        Err(err) if initialized => {
            warn_once(
                &format!("store:{agent_alias}"),
                "agent.soul_profile_unavailable",
                &format!(
                    "Soul profile for agent {agent_alias} is unavailable ({err}); \
                     running without an identity, legacy persona files stay suppressed"
                ),
            );
            return degraded_projection(configured_voice);
        }
        Err(err) => {
            warn_once(
                &format!("store:{agent_alias}"),
                "agent.soul_profile_unavailable",
                &format!(
                    "Soul profile for agent {agent_alias} could not be created ({err}); \
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

    let voice = profile
        .as_ref()
        .and_then(|profile| profile.voice.as_ref())
        .map_or(configured_voice, |head| {
            head.value.layered_over(configured_voice)
        })
        .to_prompt_section();

    let mut parts: Vec<String> = Vec::new();
    if let Some(profile) = &profile {
        parts.extend(render_identity(profile));
        parts.extend(render_principles(profile));
        parts.extend(render_growth(profile));
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

/// The projection used when an initialized Soul store cannot be read: a
/// fixed identity notice, the honesty floor, and the configured Voice.
fn degraded_projection(
    configured_voice: zeroclaw_config::persona::PersonaKnobs,
) -> PersonaProjection {
    let mut parts = vec![format!(
        "## Identity\n\n{IDENTITY_UNAVAILABLE_LINE}\n{IDENTITY_HONESTY_LINE}\n"
    )];
    parts.extend(configured_voice.to_prompt_section());
    PersonaProjection {
        section: Some(
            parts
                .iter()
                .map(|part| part.trim_end())
                .collect::<Vec<_>>()
                .join("\n\n")
                + "\n",
        ),
        legacy_files: LegacyPersonaFiles::Suppress,
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

/// Render `## Who I've become`, or `None` when the layer is absent or empty.
#[must_use]
pub fn render_growth(profile: &SoulProfile) -> Option<String> {
    let entries = &profile.growth.as_ref()?.value.entries;
    if entries.is_empty() {
        return None;
    }
    let mut out = format!("## Who I've become\n\n{GROWTH_FRAMING_LINE}\n\n");
    for (index, entry) in entries.iter().enumerate() {
        let line = match entry.kind {
            GrowthKind::SelfView => format!("- {}\n", entry.text),
            GrowthKind::Bond => format!("- Between us: {}\n", entry.text),
        };
        if out.len() + line.len() > GROWTH_SECTION_MAX_BYTES {
            let marker = format!("(+{} entries elided)\n", entries.len() - index);
            if out.len() + marker.len() <= GROWTH_SECTION_MAX_BYTES {
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
        SOUL_PROFILE_DB_FILE, SOUL_SELF_DESCRIPTION_MAX_BYTES, SOUL_SHORT_FIELD_MAX_BYTES,
        SoulIdentity, SoulPrinciples,
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
    fn store_that_cannot_be_created_falls_back_to_voice_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        // A read-only data dir: first-run creation of soul.db fails.
        let config = config_in(dir.path());
        let mut perms = std::fs::metadata(&config.data_dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&config.data_dir, perms.clone()).unwrap();
        let projection = persona_projection(&config, "nova");
        perms.set_readonly(false);
        std::fs::set_permissions(&config.data_dir, perms).unwrap();
        if config.data_dir.join(SOUL_PROFILE_DB_FILE).exists() {
            // Running as root: permissions do not stop creation.
            return;
        }
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Inject);
        assert!(
            projection
                .section
                .as_deref()
                .is_none_or(|s| !s.contains("## Identity"))
        );
    }

    #[test]
    fn unopenable_initialized_store_degrades_too() {
        let dir = tempfile::tempdir().unwrap();
        // soul.db exists but is a directory, so it cannot be opened.
        let config = config_in(dir.path());
        std::fs::create_dir_all(config.data_dir.join(SOUL_PROFILE_DB_FILE)).unwrap();
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Suppress);
        assert!(
            projection
                .section
                .unwrap()
                .contains(IDENTITY_UNAVAILABLE_LINE)
        );
    }

    /// Initialize `config`'s Soul with an owner-authored identity, using a
    /// private handle so the process-wide cache holds nothing for this dir.
    fn init_owner_identity(config: &Config) {
        let store = SoulProfileStore::open(&config.data_dir).unwrap();
        store.ensure_seeded("nova", "nova", 1).unwrap();
        store
            .set_identity(
                "nova",
                SoulIdentity {
                    name: "Nova".into(),
                    self_description: None,
                    primary_language: None,
                    pronouns: None,
                },
                1,
                2,
            )
            .unwrap();
    }

    /// #380 S13: an initialized Soul that becomes unreadable must not revive
    /// the retired legacy persona files or be replaced by a fresh store.
    #[test]
    fn unreadable_initialized_store_degrades_without_reviving_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        init_owner_identity(&config);
        let db = config.data_dir.join(SOUL_PROFILE_DB_FILE);
        let good = std::fs::read(&db).unwrap();
        std::fs::write(&db, b"this is not a sqlite database").unwrap();

        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Suppress);
        let section = projection.section.unwrap();
        assert!(section.contains(IDENTITY_UNAVAILABLE_LINE), "{section}");
        assert!(section.contains(IDENTITY_HONESTY_LINE));
        assert!(!section.contains("You are nova."));
        // No fallback store replaced the damaged one.
        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"this is not a sqlite database".to_vec()
        );

        // Once the store is readable again the owner identity is back.
        std::fs::write(&db, good).unwrap();
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Suppress);
        assert!(projection.section.unwrap().contains("You are Nova."));
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
    fn approved_growth_renders_after_principles_and_voice_layers_per_key() {
        use zeroclaw_memory::companion::{
            GrowthKind, NewSoulProposal, SoulProposalLayer, SoulProposalOutcome,
            SoulProposalResolution,
        };
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let store = SoulProfileStore::shared(&config.data_dir).unwrap();
        store.ensure_seeded("nova", "nova", 1).unwrap();
        let approve = |proposal: NewSoulProposal| {
            let SoulProposalOutcome::Recorded { id } =
                store.submit_proposal("nova", proposal, 2).unwrap()
            else {
                panic!()
            };
            store
                .resolve_proposal("nova", id, SoulProposalResolution::Accepted, None, None, 3)
                .unwrap();
        };
        approve(NewSoulProposal {
            layer: SoulProposalLayer::Growth,
            proposal: "We call a bad trade a paper cut.".into(),
            growth_kind: Some(GrowthKind::Bond),
            ..NewSoulProposal::default()
        });
        approve(NewSoulProposal {
            layer: SoulProposalLayer::Voice,
            proposal: "More levity.".into(),
            trait_key: Some("humor".into()),
            level: Some("high".into()),
            ..NewSoulProposal::default()
        });
        let section = persona_projection(&config, "nova").section.unwrap();
        let principles = section.find("## Principles").unwrap();
        let growth = section.find("## Who I've become").unwrap();
        let voice = section.find("## Voice").unwrap();
        assert!(principles < growth && growth < voice, "{section}");
        assert!(section.contains(GROWTH_FRAMING_LINE));
        assert!(section.contains("- Between us: We call a bad trade a paper cut.\n"));
        // humor=high from the approved head; no config persona is set.
        assert!(
            section.contains("Wit is welcome where it lands naturally."),
            "{section}"
        );
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
