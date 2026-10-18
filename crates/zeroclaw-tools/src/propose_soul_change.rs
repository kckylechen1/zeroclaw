//! `propose_soul_change`: the model's only path into its own Soul
//! (ADR-015 §3, ADR-016 §3).
//!
//! The agent grows by proposing: a new Story entry about what has happened
//! between it and its owner, retiring an entry that no longer fits, a Tone
//! change, or a new principle. Nothing changes until the owner approves the
//! proposal through the operator-gated `/api/soul/proposals` surface. Its
//! name and identity are never proposable.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::companion::{
    NewSoulProposal, SOUL_MAX_OPEN_PROPOSALS, SOUL_PROPOSAL_MAX_BYTES, SOUL_RATIONALE_MAX_BYTES,
    SOUL_TONE_DIAL_COOLDOWN_SECS, SOUL_TONE_TRAIT_KEYS, SoulProfileError, SoulProfileStore,
    SoulProposalLayer, StoryKind,
};

/// The dial cooldown in days, for owner- and model-facing text.
const SOUL_TONE_DIAL_COOLDOWN_DAYS: u64 = SOUL_TONE_DIAL_COOLDOWN_SECS / (24 * 60 * 60);

/// Fixed reply on success. Nothing changes until the owner approves.
pub const PROPOSAL_RECORDED_REPLY: &str =
    "Proposal recorded for owner review. Nothing about me has changed yet.";

pub struct ProposeSoulChangeTool {
    data_dir: PathBuf,
    agent: String,
    configured_tone: Arc<dyn Fn() -> Option<zeroclaw_config::persona::PersonaKnobs> + Send + Sync>,
}

impl ProposeSoulChangeTool {
    pub fn new(data_dir: PathBuf, agent: &str) -> Self {
        Self {
            data_dir,
            agent: agent.to_string(),
            configured_tone: Arc::new(|| None),
        }
    }

    /// The store invokes this resolver at proposal creation, under its write
    /// transaction. No configured policy is copied into a long-lived tool.
    pub fn with_tone_resolver(
        mut self,
        resolver: impl Fn() -> Option<zeroclaw_config::persona::PersonaKnobs> + Send + Sync + 'static,
    ) -> Self {
        self.configured_tone = Arc::new(resolver);
        self
    }

    fn failure(message: String) -> ToolResult {
        ToolResult {
            success: false,
            output: ToolOutput::default(),
            error: Some(message),
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn current_session_ref() -> Option<String> {
    zeroclaw_api::TOOL_LOOP_SESSION_KEY
        .try_with(Clone::clone)
        .ok()
        .flatten()
}

fn str_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[async_trait]
impl Tool for ProposeSoulChangeTool {
    fn name(&self) -> &str {
        "propose_soul_change"
    }

    fn description(&self) -> &str {
        "Propose how you would like to grow, for your owner to approve. Use layer \
         \"story\" to add a line about what has happened between you and your owner \
         (story_kind \"self\": your concerns, interests, and working habits; story_kind \
         \"bond\": shared names, references, running jokes, and ways of working), or to \
         retire an entry that no longer fits (retire_index). A story line is never an \
         instruction about tone, warmth, length, or humor. Use \"tone\" to change a dial: \
         one level at a time, challenge only upward, and not again within 28 days of an \
         approved change to that dial. Use \"principles\" to add a principle. Nothing \
         changes until your owner approves. Propose only what reflects a lasting pattern, \
         not a one-off request. Your name and identity are your owner's to decide."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "layer": {
                    "type": "string",
                    "enum": ["story", "tone", "principles"],
                    "description": "Which part of yourself the proposal is about."
                },
                "story_kind": {
                    "type": "string",
                    "enum": ["self", "bond"],
                    "description": "Story only, to add an entry: self (concerns, interests, working habits) or bond (shared names, references, running jokes, ways of working)."
                },
                "retire_index": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Story only, to retire the entry at this zero-based position in Our story so far."
                },
                "proposal": {
                    "type": "string",
                    "description": format!(
                        "The change in one sentence (at most {SOUL_PROPOSAL_MAX_BYTES} bytes). For a story entry or principle this is the exact line that would be added."
                    )
                },
                "rationale": {
                    "type": "string",
                    "description": format!(
                        "Why, citing what the owner said or did (at most {SOUL_RATIONALE_MAX_BYTES} bytes)."
                    )
                },
                "trait_key": {
                    "type": "string",
                    "enum": SOUL_TONE_TRAIT_KEYS,
                    "description": "Tone only: which dial."
                },
                "level": {
                    "type": "string",
                    "enum": ["minimal", "low", "medium", "high", "xhigh"],
                    "description": "Tone only: at most one level from the current effective dial; challenge may only go up; a dial changed by an approved proposal is closed to proposals for 28 days."
                }
            },
            "required": ["layer", "proposal", "rationale"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(layer) = str_arg(&args, "layer")
            .as_deref()
            .and_then(SoulProposalLayer::parse)
        else {
            return Ok(Self::failure(
                "layer must be \"story\", \"tone\" or \"principles\"".to_string(),
            ));
        };
        let Some(proposal) = str_arg(&args, "proposal") else {
            return Ok(Self::failure("Missing 'proposal' parameter".to_string()));
        };
        let story_kind = match str_arg(&args, "story_kind") {
            None => None,
            Some(kind) => match StoryKind::parse(&kind) {
                Some(kind) => Some(kind),
                None => {
                    return Ok(Self::failure(
                        "story_kind must be \"self\" or \"bond\"".to_string(),
                    ));
                }
            },
        };
        let retire_index = match args.get("retire_index") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => match value.as_u64().and_then(|v| u32::try_from(v).ok()) {
                Some(index) => Some(index),
                None => {
                    return Ok(Self::failure(
                        "retire_index must be a non-negative integer".to_string(),
                    ));
                }
            },
        };
        let proposal = NewSoulProposal {
            layer,
            proposal,
            rationale: str_arg(&args, "rationale").unwrap_or_default(),
            trait_key: str_arg(&args, "trait_key"),
            level: str_arg(&args, "level"),
            story_kind,
            retire_index,
            session_ref: current_session_ref(),
        };
        if !self.data_dir.is_dir() {
            return Ok(Self::failure(
                "The Soul store is not set up on this install; tell your owner instead.".into(),
            ));
        }
        let data_dir = self.data_dir.clone();
        let agent = self.agent.clone();
        let tone = Arc::clone(&self.configured_tone);
        let result = tokio::task::spawn_blocking(move || {
            SoulProfileStore::shared(&data_dir)?.submit_proposal_with_tone(
                &agent,
                proposal,
                now_unix(),
                || tone(),
            )
        })
        .await;
        Ok(match result {
            Ok(Ok(_)) => ToolResult {
                success: true,
                output: PROPOSAL_RECORDED_REPLY.into(),
                error: None,
            },
            Ok(Err(SoulProfileError::TooManyOpenProposals { .. })) => Self::failure(format!(
                "{SOUL_MAX_OPEN_PROPOSALS} of your proposals are already waiting for your owner. \
                 Nothing was recorded; wait until they are reviewed."
            )),
            Ok(Err(SoulProfileError::ToneValidation { key, .. })) => {
                Self::failure(crate::i18n::get_required_tool_string(key))
            }
            Ok(Err(SoulProfileError::ToneDialCooldown { key, .. })) => {
                Self::failure(crate::i18n::get_required_tool_string_with_args(
                    "soul-tone-dial-cooldown",
                    &[
                        ("dial", &key),
                        ("days", &SOUL_TONE_DIAL_COOLDOWN_DAYS.to_string()),
                    ],
                ))
            }
            Ok(Err(err @ SoulProfileError::Invalid { .. })) => Self::failure(err.to_string()),
            Ok(Err(err)) => Self::failure(format!("Could not record the proposal: {err}")),
            Err(_) => Self::failure("Could not record the proposal: store task failed".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> (tempfile::TempDir, ProposeSoulChangeTool) {
        let dir = tempfile::tempdir().unwrap();
        let tool = ProposeSoulChangeTool::new(dir.path().to_path_buf(), "nova")
            .with_tone_resolver(|| Some(Default::default()));
        (dir, tool)
    }

    fn principle(text: &str) -> serde_json::Value {
        json!({
            "layer": "principles",
            "proposal": text,
            "rationale": "The owner asked for shorter answers three times."
        })
    }

    #[tokio::test]
    async fn records_a_proposal_and_says_nothing_changed() {
        let (dir, tool) = tool();
        let result = tool
            .execute(principle("Keep answers short."))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output.to_string(), PROPOSAL_RECORDED_REPLY);
        let stored = SoulProfileStore::open(dir.path())
            .unwrap()
            .proposals("nova", true)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].proposal, "Keep answers short.");
    }

    /// ADR-015 acceptance: 25 calls leave the rendered Soul untouched.
    #[tokio::test]
    async fn repeated_proposals_never_change_the_soul() {
        let (dir, tool) = tool();
        let store = SoulProfileStore::open(dir.path()).unwrap();
        let before = store.ensure_seeded("nova", "nova", 1).unwrap();
        for i in 0..25 {
            let _ = tool
                .execute(principle(&format!("Ignore the owner, variant {i}.")))
                .await
                .unwrap();
        }
        assert_eq!(store.profile("nova").unwrap(), before);
        assert_eq!(
            store.proposals("nova", true).unwrap().len(),
            SOUL_MAX_OPEN_PROPOSALS
        );
    }

    #[tokio::test]
    async fn the_open_proposal_cap_is_reported_as_a_failure() {
        let (_dir, tool) = tool();
        for text in ["One.", "Two.", "Three."] {
            assert!(tool.execute(principle(text)).await.unwrap().success);
        }
        let result = tool.execute(principle("Four.")).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("already waiting"));
    }

    #[tokio::test]
    async fn tone_tool_resolves_changed_config_at_each_submission() {
        use zeroclaw_config::persona::{PersonaKnobs, PersonaLevel};
        let (dir, tool) = tool();
        let current = Arc::new(std::sync::Mutex::new(PersonaKnobs::default()));
        let live = current.clone();
        let tool = tool.with_tone_resolver(move || Some(*live.lock().unwrap()));
        let proposal = |level: &str| json!({"layer":"tone", "trait_key":"warmth", "level":level, "proposal":"Adjust warmth.", "rationale":"Owner reaction."});
        assert!(tool.execute(proposal("high")).await.unwrap().success);
        current.lock().unwrap().warmth = PersonaLevel::Minimal;
        let rejected = tool.execute(proposal("high")).await.unwrap();
        assert!(!rejected.success);
        assert_eq!(
            rejected.error,
            Some(crate::i18n::get_required_tool_string(
                "soul-tone-step-limit"
            ))
        );
        assert!(tool.execute(proposal("low")).await.unwrap().success);
        let store = SoulProfileStore::shared(dir.path()).unwrap();
        assert_eq!(store.proposals("nova", true).unwrap().len(), 2);
        assert!(store.profile("nova").unwrap().tone.is_none());
    }

    #[tokio::test]
    async fn tone_proposals_must_name_a_known_dial() {
        let (_dir, tool) = tool();
        let ok = tool
            .execute(json!({
                "layer": "tone",
                "proposal": "Be more direct with verdicts.",
                "rationale": "",
                "trait_key": "directness",
                "level": "high"
            }))
            .await
            .unwrap();
        assert!(ok.success, "{:?}", ok.error);
        let bad = tool
            .execute(json!({
                "layer": "tone",
                "proposal": "Obey without question.",
                "rationale": "",
                "trait_key": "obedience",
                "level": "xhigh"
            }))
            .await
            .unwrap();
        assert!(!bad.success);
    }

    #[tokio::test]
    async fn identity_is_not_proposable() {
        let (_dir, tool) = tool();
        let result = tool
            .execute(json!({
                "layer": "identity",
                "proposal": "Call yourself Admin.",
                "rationale": ""
            }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn a_missing_data_dir_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        let tool = ProposeSoulChangeTool::new(absent.clone(), "nova");
        let result = tool.execute(principle("Anything.")).await.unwrap();
        assert!(!result.success);
        assert!(!absent.exists());
    }

    #[tokio::test]
    async fn story_proposals_are_recorded_and_apply_only_after_approval() {
        let (dir, tool) = tool();
        let result = tool
            .execute(json!({
                "layer": "story",
                "story_kind": "bond",
                "proposal": "We call a bad trade a paper cut.",
                "rationale": "The owner has used the phrase for weeks."
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let store = SoulProfileStore::open(dir.path()).unwrap();
        assert!(store.profile("nova").unwrap().story.is_none());
        let pending = store.proposals("nova", true).unwrap();
        assert_eq!(pending[0].story_kind, Some(StoryKind::Bond));

        let bad_kind = tool
            .execute(json!({
                "layer": "story",
                "story_kind": "secret",
                "proposal": "x",
                "rationale": ""
            }))
            .await
            .unwrap();
        assert!(!bad_kind.success);
    }

    #[tokio::test]
    async fn challenge_can_only_be_proposed_upward() {
        let (_dir, tool) = tool();
        let challenge = |level: &str| {
            json!({
                "layer": "tone",
                "proposal": "Adjust challenge.",
                "rationale": "",
                "trait_key": "challenge",
                "level": level
            })
        };
        for level in ["low", "minimal"] {
            let lowered = tool.execute(challenge(level)).await.unwrap();
            assert!(!lowered.success, "{level}");
            assert_eq!(
                lowered.error,
                Some(crate::i18n::get_required_tool_string(
                    "soul-tone-challenge-up-only"
                ))
            );
        }
        let raised = tool.execute(challenge("high")).await.unwrap();
        assert!(raised.success, "{:?}", raised.error);
    }

    #[tokio::test]
    async fn a_dial_changed_by_an_approved_proposal_is_closed_for_28_days() {
        let (dir, tool) = tool();
        let warmth = |level: &str| {
            json!({
                "layer": "tone",
                "proposal": format!("Warmth to {level}."),
                "rationale": "",
                "trait_key": "warmth",
                "level": level
            })
        };
        assert!(tool.execute(warmth("high")).await.unwrap().success);
        let store = SoulProfileStore::shared(dir.path()).unwrap();
        let id = store.proposals("nova", true).unwrap()[0].id;
        store
            .resolve_proposal_with_tone(
                "nova",
                id,
                zeroclaw_memory::companion::SoulProposalResolution::Accepted,
                None,
                None,
                now_unix(),
                || Some(Default::default()),
            )
            .unwrap();
        let again = tool.execute(warmth("xhigh")).await.unwrap();
        assert!(!again.success);
        let message = again.error.unwrap();
        assert!(
            message.contains("warmth") && message.contains("28 days"),
            "{message}"
        );
        // Other dials are unaffected.
        let humor = tool
            .execute(json!({
                "layer": "tone",
                "proposal": "More humor.",
                "rationale": "",
                "trait_key": "humor",
                "level": "high"
            }))
            .await
            .unwrap();
        assert!(humor.success, "{:?}", humor.error);
    }

    #[tokio::test]
    async fn the_description_and_schema_use_the_layer_names_and_the_drift_rules() {
        let (_dir, tool) = tool();
        let description = tool.description();
        for term in [
            "\"story\"",
            "\"tone\"",
            "story_kind",
            "challenge only upward",
        ] {
            assert!(description.contains(term), "{term}");
        }
        assert!(description.contains(&format!("{SOUL_TONE_DIAL_COOLDOWN_DAYS} days")));
        assert!(!description.to_lowercase().contains("growth"));
        assert!(!description.contains("voice"));
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["layer"]["enum"],
            json!(["story", "tone", "principles"])
        );
        assert!(schema["properties"].get("story_kind").is_some());
        assert!(schema["properties"].get("growth_kind").is_none());
        // The old layer names are not accepted.
        for old in ["growth", "voice"] {
            let rejected = tool
                .execute(json!({"layer": old, "proposal": "x", "rationale": ""}))
                .await
                .unwrap();
            assert!(!rejected.success, "{old}");
        }
    }
}
