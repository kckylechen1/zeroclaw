//! `propose_soul_change`: the model's only path into its own Soul
//! (ADR-015 §3, ADR-016 §3).
//!
//! The agent grows by proposing: a new Growth entry about itself or about
//! what it shares with its owner, retiring an entry that no longer fits, a
//! Voice change, or a new principle. Nothing changes until the owner approves
//! the proposal through the operator-gated `/api/soul/proposals` surface. Its
//! name and identity are never proposable.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::companion::{
    GrowthKind, NewSoulProposal, SOUL_MAX_OPEN_PROPOSALS, SOUL_PROPOSAL_MAX_BYTES,
    SOUL_RATIONALE_MAX_BYTES, SOUL_VOICE_TRAIT_KEYS, SoulProfileError, SoulProfileStore,
    SoulProposalLayer,
};

/// Fixed reply on success. Nothing changes until the owner approves.
pub const PROPOSAL_RECORDED_REPLY: &str =
    "Proposal recorded for owner review. Nothing about me has changed yet.";

pub struct ProposeSoulChangeTool {
    data_dir: PathBuf,
    agent: String,
}

impl ProposeSoulChangeTool {
    pub fn new(data_dir: PathBuf, agent: &str) -> Self {
        Self {
            data_dir,
            agent: agent.to_string(),
        }
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
         \"growth\" to add a line about who you have become (growth_kind \"self\") or \
         about something you and your owner share, such as a nickname or a running joke \
         (growth_kind \"bond\"), or to retire an entry that no longer fits (retire_index). \
         Use \"voice\" to change a dial and \"principles\" to add a principle. Nothing \
         changes until your owner approves. Propose only what reflects a lasting pattern, \
         not a one-off request. Your name and identity are your owner's to decide."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "layer": {
                    "type": "string",
                    "enum": ["growth", "voice", "principles"],
                    "description": "Which part of yourself the proposal is about."
                },
                "growth_kind": {
                    "type": "string",
                    "enum": ["self", "bond"],
                    "description": "Growth only, to add an entry: self (how you have changed) or bond (what you and your owner share)."
                },
                "retire_index": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Growth only, to retire the entry at this zero-based position in Who I've become."
                },
                "proposal": {
                    "type": "string",
                    "description": format!(
                        "The change in one sentence (at most {SOUL_PROPOSAL_MAX_BYTES} bytes). For a growth entry or principle this is the exact line that would be added."
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
                    "enum": SOUL_VOICE_TRAIT_KEYS,
                    "description": "Voice only: which dial."
                },
                "level": {
                    "type": "string",
                    "enum": ["minimal", "low", "medium", "high", "xhigh"],
                    "description": "Voice only: the suggested position."
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
                "layer must be \"growth\", \"voice\" or \"principles\"".to_string(),
            ));
        };
        let Some(proposal) = str_arg(&args, "proposal") else {
            return Ok(Self::failure("Missing 'proposal' parameter".to_string()));
        };
        let growth_kind = match str_arg(&args, "growth_kind") {
            None => None,
            Some(kind) => match GrowthKind::parse(&kind) {
                Some(kind) => Some(kind),
                None => {
                    return Ok(Self::failure(
                        "growth_kind must be \"self\" or \"bond\"".to_string(),
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
            growth_kind,
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
        let result = tokio::task::spawn_blocking(move || {
            SoulProfileStore::shared(&data_dir)?.submit_proposal(&agent, proposal, now_unix())
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
        let tool = ProposeSoulChangeTool::new(dir.path().to_path_buf(), "nova");
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
    async fn voice_proposals_must_name_a_known_dial() {
        let (_dir, tool) = tool();
        let ok = tool
            .execute(json!({
                "layer": "voice",
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
                "layer": "voice",
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
    async fn growth_proposals_are_recorded_and_apply_only_after_approval() {
        let (dir, tool) = tool();
        let result = tool
            .execute(json!({
                "layer": "growth",
                "growth_kind": "bond",
                "proposal": "We call a bad trade a paper cut.",
                "rationale": "The owner has used the phrase for weeks."
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let store = SoulProfileStore::open(dir.path()).unwrap();
        assert!(store.profile("nova").unwrap().growth.is_none());
        let pending = store.proposals("nova", true).unwrap();
        assert_eq!(pending[0].growth_kind, Some(GrowthKind::Bond));

        let bad_kind = tool
            .execute(json!({
                "layer": "growth",
                "growth_kind": "secret",
                "proposal": "x",
                "rationale": ""
            }))
            .await
            .unwrap();
        assert!(!bad_kind.success);
        let lower_challenge = tool
            .execute(json!({
                "layer": "voice",
                "proposal": "Stop arguing.",
                "rationale": "",
                "trait_key": "challenge",
                "level": "minimal"
            }))
            .await
            .unwrap();
        assert!(!lower_challenge.success);
    }
}
