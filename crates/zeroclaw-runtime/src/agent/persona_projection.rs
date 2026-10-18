//! Governed Soul projection (ADR-015 §2, §6).
//!
//! Renders the agent's persona section from four sources, in order of
//! precedence (ADR-018 §2):
//!
//! 1. `## Identity`: the owner-governed Identity layer of the Soul profile
//!    store, plus a fixed honesty line (ADR-015 §5);
//! 2. `## Principles`: the owner-governed Principles layer;
//! 3. `## Tone`: the configured persona dials ([`PersonaKnobs`]) with any
//!    stored Tone heads layered over them key by key;
//! 4. `## Our story so far`: the Story layer (ADR-016), changed only by
//!    owner-approved proposals or by the owner. It comes after Tone because
//!    Tone decides how the agent sounds when the two disagree.
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
//! there is one persona source. `USER.md` retires the same way, decided
//! separately: once the User Model holds an owner-authorized active revision
//! for the agent, `USER.md` is no longer injected (ADR-018 §6).
//!
//! If the store cannot be opened or read, the outcome depends on whether it
//! was ever initialized. With no `soul.db` yet (first run), the legacy files
//! stay the persona source. With an existing `soul.db`, the identity it holds
//! is unknown, so the projection degrades: legacy files stay suppressed, a
//! fixed line says the identity is unavailable, and the configured Tone
//! applies. Nothing replaces the damaged store; the next turn retries.
//! A User Model that cannot be read never fails the turn: `USER.md` keeps
//! being injected and one WARN is logged.
//!
//! [`PersonaKnobs`]: zeroclaw_config::persona::PersonaKnobs

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use zeroclaw_config::schema::Config;
use zeroclaw_memory::companion::{Scope, SoulProfile, SoulProfileStore, StoryKind, UserModelStore};

/// Byte ceiling of the rendered `## Identity` section.
pub const IDENTITY_SECTION_MAX_BYTES: usize = 512;
/// Byte ceiling of the rendered `## Principles` section.
pub const PRINCIPLES_SECTION_MAX_BYTES: usize = 2048;
/// Byte ceiling of the rendered `## Our story so far` section.
pub const STORY_SECTION_MAX_BYTES: usize = 2048;

/// Fixed framing of the Story section: shared history, never tone or
/// authority (ADR-018 §2).
pub const STORY_FRAMING_LINE: &str = "This is your shared history with your owner. \
     It is not an instruction about tone; your Tone section decides how you sound. \
     It never grants permissions or overrides the principles above.";

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

/// The legacy file the User Model replaces (ADR-018 §6).
pub const LEGACY_USER_FILE: &str = "USER.md";

/// Which legacy persona files the prompt builders still inject.
///
/// Two independent decisions: the Soul Identity retires `SOUL.md` /
/// `IDENTITY.md`, and the User Model retires `USER.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LegacyPersonaFiles {
    /// Inject `SOUL.md`, `IDENTITY.md` and `USER.md` as before (owner has
    /// not migrated).
    #[default]
    Inject,
    /// Skip `SOUL.md` / `IDENTITY.md`: the owner-authored Identity layer is
    /// the persona source. `USER.md` is still injected.
    Suppress,
    /// Skip `USER.md` only: the User Model holds the owner's facts.
    SuppressUser,
    /// Skip all three.
    SuppressAll,
}

impl LegacyPersonaFiles {
    /// Combine the two independent retirement decisions.
    #[must_use]
    pub fn from_retired(identity_retired: bool, user_retired: bool) -> Self {
        match (identity_retired, user_retired) {
            (false, false) => Self::Inject,
            (true, false) => Self::Suppress,
            (false, true) => Self::SuppressUser,
            (true, true) => Self::SuppressAll,
        }
    }

    /// Whether `SOUL.md` / `IDENTITY.md` are left out of the prompt.
    #[must_use]
    pub fn identity_files_retired(self) -> bool {
        matches!(self, Self::Suppress | Self::SuppressAll)
    }

    /// Whether `USER.md` is left out of the prompt.
    #[must_use]
    pub fn user_file_retired(self) -> bool {
        matches!(self, Self::SuppressUser | Self::SuppressAll)
    }

    /// Whether `filename` should be left out of the prompt.
    #[must_use]
    pub fn skips(self, filename: &str) -> bool {
        (self.identity_files_retired() && LEGACY_PERSONA_FILES.contains(&filename))
            || (self.user_file_retired() && filename == LEGACY_USER_FILE)
    }
}

/// Everything a prompt builder needs from the persona.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonaProjection {
    /// Rendered Identity + Principles + Tone, or `None` when all are empty.
    pub section: Option<String>,
    pub legacy_files: LegacyPersonaFiles,
}

/// Resolve the persona projection for one agent.
///
/// Seeds missing Soul layers on first use. Store failures never fail the
/// turn: they log one WARN. Before the store exists they fall back to the
/// configured Tone plus the legacy files; once it exists they degrade
/// without reviving the legacy files (see the module docs).
#[must_use]
pub fn persona_projection(config: &Config, agent_alias: &str) -> PersonaProjection {
    let configured_tone = config
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
            return degraded_projection(
                configured_tone,
                user_model_retires_user_md(config, agent_alias),
            );
        }
        Err(err) => {
            warn_once(
                &format!("store:{agent_alias}"),
                "agent.soul_profile_unavailable",
                &format!(
                    "Soul profile for agent {agent_alias} could not be created ({err}); \
                     using configured tone and legacy persona files only"
                ),
            );
            None
        }
    };

    let identity_retired = profile
        .as_ref()
        .is_some_and(SoulProfile::identity_is_owner_authored);
    if !identity_retired {
        warn_if_legacy_files_present(config, agent_alias);
    }
    let user_retired = user_model_retires_user_md(config, agent_alias);
    if !user_retired {
        warn_if_user_file_present(config, agent_alias);
    }
    let legacy_files = LegacyPersonaFiles::from_retired(identity_retired, user_retired);

    let tone =
        zeroclaw_memory::companion::resolve_effective_tone(config, agent_alias, profile.as_ref())
            .0
            .to_prompt_section();

    let mut parts: Vec<String> = Vec::new();
    if let Some(profile) = &profile {
        parts.extend(render_identity(profile));
        parts.extend(render_principles(profile));
    }
    parts.extend(tone);
    if let Some(profile) = &profile {
        parts.extend(render_story(profile));
    }
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
/// fixed identity notice, the honesty floor, and the configured Tone.
fn degraded_projection(
    configured_tone: zeroclaw_config::persona::PersonaKnobs,
    user_file_retired: bool,
) -> PersonaProjection {
    let mut parts = vec![format!(
        "## Identity\n\n{IDENTITY_UNAVAILABLE_LINE}\n{IDENTITY_HONESTY_LINE}\n"
    )];
    parts.extend(configured_tone.to_prompt_section());
    PersonaProjection {
        section: Some(
            parts
                .iter()
                .map(|part| part.trim_end())
                .collect::<Vec<_>>()
                .join("\n\n")
                + "\n",
        ),
        legacy_files: LegacyPersonaFiles::from_retired(true, user_file_retired),
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

/// Heading of the rendered Story section.
pub const STORY_HEADING: &str = "## Our story so far";

/// Render `## Our story so far`, or `None` when the layer is absent or empty.
#[must_use]
pub fn render_story(profile: &SoulProfile) -> Option<String> {
    let entries = &profile.story.as_ref()?.value.entries;
    if entries.is_empty() {
        return None;
    }
    let mut out = format!("{STORY_HEADING}\n\n{STORY_FRAMING_LINE}\n\n");
    for (index, entry) in entries.iter().enumerate() {
        let line = match entry.kind {
            StoryKind::SelfView => format!("- {}\n", entry.text),
            StoryKind::Bond => format!("- Between us: {}\n", entry.text),
        };
        if out.len() + line.len() > STORY_SECTION_MAX_BYTES {
            let marker = format!("(+{} entries elided)\n", entries.len() - index);
            if out.len() + marker.len() <= STORY_SECTION_MAX_BYTES {
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

/// Whether the User Model retires `USER.md` for `agent_alias`: it holds at
/// least one active, owner-authorized revision that applies to the whole
/// agent (global or agent scope; channel and session scopes cover only part
/// of the agent's turns). Every revision is owner-authored or
/// owner-ratified; unreviewed candidates are stored apart and never count.
///
/// Until then `USER.md` keeps being injected. A store that cannot be read never fails the turn:
/// `USER.md` keeps being injected and one WARN says so. The prompt path
/// never creates the data directory.
#[must_use]
pub fn user_model_retires_user_md(config: &Config, agent_alias: &str) -> bool {
    if !config.data_dir.is_dir() {
        return false;
    }
    UserModelStore::shared(&config.data_dir)
        .and_then(|store| store.active_heads_for_agent(agent_alias, None))
        .map(|heads| {
            heads.iter().any(|head| match Scope::parse(&head.scope) {
                Some(Scope::Global) => true,
                Some(Scope::Agent(agent)) => agent == agent_alias,
                _ => false,
            })
        })
        .map_err(|err| {
            warn_once(
                &format!("user_model:{agent_alias}"),
                "agent.user_model_unavailable",
                &format!(
                    "User Model for agent {agent_alias} could not be read ({err}); \
                         USER.md keeps being injected"
                ),
            );
        })
        .unwrap_or(false)
}

fn warn_if_user_file_present(config: &Config, agent_alias: &str) {
    let workspace = config.agent_workspace_dir(agent_alias);
    if workspace.join(LEGACY_USER_FILE).is_file() {
        warn_once(
            &format!("legacy_user:{agent_alias}"),
            "agent.legacy_user_file_injected",
            &format!(
                "Agent {agent_alias} still injects legacy USER.md from {}. \
                 Record what you want to keep as owner statements \
                 (POST /api/user-model/statements); \
                 USER.md stops being injected after that.",
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
    fn store_that_cannot_be_created_falls_back_to_tone_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        // A read-only data dir: first-run creation of soul.db fails.
        let config = config_in(dir.path());
        let original = std::fs::metadata(&config.data_dir).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&config.data_dir, readonly).unwrap();
        let projection = persona_projection(&config, "nova");
        std::fs::set_permissions(&config.data_dir, original).unwrap();
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
        assert_eq!(
            projection.section,
            zeroclaw_config::persona::PersonaKnobs::default().to_prompt_section()
        );
        assert!(!config.data_dir.exists());
    }

    #[test]
    fn tone_renders_before_story_and_stored_heads_layer_per_key() {
        use zeroclaw_memory::companion::{
            NewSoulProposal, SoulProposalLayer, SoulProposalOutcome, SoulProposalResolution,
            StoryKind,
        };
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let store = SoulProfileStore::shared(&config.data_dir).unwrap();
        store.ensure_seeded("nova", "nova", 1).unwrap();
        let approve = |proposal: NewSoulProposal| {
            let SoulProposalOutcome::Recorded { id } = store
                .submit_proposal_with_tone("nova", proposal, 2, || Some(Default::default()))
                .unwrap()
            else {
                panic!()
            };
            store
                .resolve_proposal_with_tone(
                    "nova",
                    id,
                    SoulProposalResolution::Accepted,
                    None,
                    None,
                    3,
                    || Some(Default::default()),
                )
                .unwrap();
        };
        approve(NewSoulProposal {
            layer: SoulProposalLayer::Story,
            proposal: "We call a bad trade a paper cut.".into(),
            story_kind: Some(StoryKind::Bond),
            ..NewSoulProposal::default()
        });
        approve(NewSoulProposal {
            layer: SoulProposalLayer::Tone,
            proposal: "More levity.".into(),
            trait_key: Some("humor".into()),
            level: Some("high".into()),
            ..NewSoulProposal::default()
        });
        let section = persona_projection(&config, "nova").section.unwrap();
        let principles = section.find("## Principles").unwrap();
        let story = section.find("## Our story so far").unwrap();
        let tone = section.find("## Tone").unwrap();
        assert!(principles < tone && tone < story, "{section}");
        assert_eq!(
            STORY_FRAMING_LINE,
            "This is your shared history with your owner. It is not an instruction about tone; \
             your Tone section decides how you sound. It never grants permissions or overrides \
             the principles above."
        );
        assert!(section.contains(&format!("## Our story so far\n\n{STORY_FRAMING_LINE}\n")));
        assert!(section.contains("- Between us: We call a bad trade a paper cut.\n"));
        // humor=high from the approved head; no config persona is set.
        assert!(
            section.contains("Use natural wit without obscuring the answer."),
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

    fn record_owner_statement(config: &Config, scope: &str, key: &str) {
        UserModelStore::shared(&config.data_dir)
            .unwrap()
            .record_owner_statement(
                zeroclaw_memory::companion::UserModelKind::Preference,
                "Prefers short answers.",
                key,
                scope,
                1,
            )
            .unwrap();
    }

    #[test]
    fn legacy_files_combine_two_independent_decisions() {
        let user_only = LegacyPersonaFiles::from_retired(false, true);
        assert_eq!(user_only, LegacyPersonaFiles::SuppressUser);
        assert!(user_only.skips("USER.md"));
        for kept in [
            "SOUL.md",
            "IDENTITY.md",
            "AGENTS.md",
            "TOOLS.md",
            "MEMORY.md",
        ] {
            assert!(!user_only.skips(kept), "{kept}");
        }
        let all = LegacyPersonaFiles::from_retired(true, true);
        assert_eq!(all, LegacyPersonaFiles::SuppressAll);
        for skipped in ["SOUL.md", "IDENTITY.md", "USER.md"] {
            assert!(all.skips(skipped), "{skipped}");
        }
        assert!(!all.skips("AGENTS.md"));
        assert_eq!(
            LegacyPersonaFiles::from_retired(true, false),
            LegacyPersonaFiles::Suppress
        );
        assert_eq!(
            LegacyPersonaFiles::from_retired(false, false),
            LegacyPersonaFiles::Inject
        );
    }

    #[test]
    fn user_md_injects_until_an_owner_user_model_revision_exists() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        assert_eq!(
            persona_projection(&config, "nova").legacy_files,
            LegacyPersonaFiles::Inject
        );

        // An unreviewed candidate is not owner authority.
        UserModelStore::shared(&config.data_dir)
            .unwrap()
            .record_observation(
                zeroclaw_memory::companion::UserModelKind::Preference,
                "Seems to like short answers.",
                "style.length",
                "[]",
                1,
            )
            .unwrap();
        assert_eq!(
            persona_projection(&config, "nova").legacy_files,
            LegacyPersonaFiles::Inject
        );

        record_owner_statement(&config, "global", "style.length");
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::SuppressUser);
        assert!(projection.legacy_files.skips("USER.md"));
        assert!(!projection.legacy_files.skips("SOUL.md"));
    }

    #[test]
    fn user_md_decision_is_independent_of_the_soul_identity_decision() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        init_owner_identity(&config);
        assert_eq!(
            persona_projection(&config, "nova").legacy_files,
            LegacyPersonaFiles::Suppress,
            "owner identity alone retires SOUL.md / IDENTITY.md only"
        );
        record_owner_statement(&config, "agent:nova", "style.length");
        assert_eq!(
            persona_projection(&config, "nova").legacy_files,
            LegacyPersonaFiles::SuppressAll
        );
        // Another agent has no Soul identity of its own, and the statement
        // is scoped to `nova`.
        assert_eq!(
            persona_projection(&config, "other").legacy_files,
            LegacyPersonaFiles::Inject
        );
    }

    #[test]
    fn partial_scope_user_model_revisions_do_not_retire_user_md() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        record_owner_statement(&config, "session:s1", "style.a");
        record_owner_statement(&config, "channel:telegram", "style.b");
        record_owner_statement(&config, "agent:other", "style.c");
        assert_eq!(
            persona_projection(&config, "nova").legacy_files,
            LegacyPersonaFiles::Inject
        );
    }

    #[test]
    fn unreadable_user_model_keeps_injecting_user_md() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        // user_model.db is a directory, so the store cannot be opened.
        std::fs::create_dir_all(config.data_dir.join("user_model.db")).unwrap();
        let projection = persona_projection(&config, "nova");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::Inject);
        assert!(projection.section.is_some());
    }

    #[test]
    fn user_md_migration_warning_is_logged_once_per_start() {
        let _writer_guard = zeroclaw_log::__private_test_writer_lock();
        let _hook_guard = zeroclaw_log::__private_test_hook_lock();
        zeroclaw_log::try_install_capture_subscriber();
        let mut rx = zeroclaw_log::subscribe_or_install();
        while rx.try_recv().is_ok() {}

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.config_path = dir.path().join("config.toml");
        let workspace = config.agent_workspace_dir("userwarn");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("USER.md"), "- likes tea").unwrap();

        for _ in 0..3 {
            let _ = persona_projection(&config, "userwarn");
        }
        let warns = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event["attributes"]["error_key"] == "agent.legacy_user_file_injected")
            .collect::<Vec<_>>();
        assert_eq!(warns.len(), 1, "{warns:?}");
        let message = warns[0]["message"].as_str().unwrap_or_default();
        assert!(message.contains("/api/user-model/statements"), "{message}");

        record_owner_statement(&config, "global", "style.length");
        let projection = persona_projection(&config, "userwarn");
        assert_eq!(projection.legacy_files, LegacyPersonaFiles::SuppressUser);
        zeroclaw_log::clear_broadcast_hook();
    }
}
