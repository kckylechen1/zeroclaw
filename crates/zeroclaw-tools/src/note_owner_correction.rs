//! Session-bound User Model proposals. Owner review is the only promotion path.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use zeroclaw_api::companion::{
    AuthorityClass, CompanionIngress, CompanionOwnerGate, IngressIdentity,
    classify_companion_authority,
};
use zeroclaw_api::review::{OWNER_CORRECTION_CONTEXT, UserMessageSource};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::companion::{UserModelKind, UserModelStore};

/// Resolves the canonical owner policy and storage root at use time.
type OwnerResolver = Arc<dyn Fn() -> Option<(CompanionOwnerGate, PathBuf)> + Send + Sync>;

pub struct NoteOwnerCorrectionTool {
    agent: String,
    resolve: OwnerResolver,
}

impl NoteOwnerCorrectionTool {
    pub fn new(
        agent: &str,
        resolve: impl Fn() -> Option<(CompanionOwnerGate, PathBuf)> + Send + Sync + 'static,
    ) -> Self {
        Self {
            agent: agent.to_string(),
            resolve: Arc::new(resolve),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Correction {
    kind: UserModelKind,
    statement: String,
    semantic_key: String,
}

fn text(key: &str) -> String {
    crate::i18n::get_required_tool_string(key)
}

fn denied(key: &str) -> ToolResult {
    ToolResult {
        success: false,
        output: ToolOutput::default(),
        error: Some(text(key)),
    }
}

#[async_trait]
impl Tool for NoteOwnerCorrectionTool {
    fn name(&self) -> &str {
        "note_owner_correction"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: OnceLock<String> = OnceLock::new();
        DESCRIPTION.get_or_init(|| text("tool-note-owner-correction"))
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "kind": {"type": "string", "enum": ["value", "goal", "preference", "habit", "constraint"]},
                "statement": {"type": "string", "description": text("tool-owner-correction-statement")},
                "semantic_key": {"type": "string", "description": text("tool-owner-correction-key")}
            },
            "required": ["kind", "statement", "semantic_key"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Ok(correction) = serde_json::from_value::<Correction>(args) else {
            return Ok(denied("tool-owner-correction-invalid"));
        };
        let context = OWNER_CORRECTION_CONTEXT
            .try_with(|resolve| resolve())
            .ok()
            .flatten();
        let session = zeroclaw_api::TOOL_LOOP_SESSION_KEY
            .try_with(Clone::clone)
            .ok()
            .flatten();
        let Some(context) = context.filter(|c| {
            c.agent_alias == self.agent
                && !c.session_key.trim().is_empty()
                && session.as_deref() == Some(c.session_key.as_str())
        }) else {
            return Ok(denied("tool-owner-correction-denied"));
        };
        let Some((owner, data_dir)) = (self.resolve)() else {
            return Ok(denied("tool-owner-correction-denied"));
        };
        let admitted = match &context.ingress.source {
            UserMessageSource::Operator => true,
            UserMessageSource::Channel { sender_id } => {
                classify_companion_authority(
                    &CompanionIngress::from_channel_identity(IngressIdentity::new(sender_id)),
                    &owner,
                ) == AuthorityClass::OwnerAuthored
            }
        };
        if !admitted || !data_dir.is_dir() {
            return Ok(denied("tool-owner-correction-denied"));
        }
        let Some(owner_text) =
            zeroclaw_memory::companion::reflection::owner_text(&context.ingress.text)
        else {
            return Ok(denied("tool-owner-correction-denied"));
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let evidence = json!({"origin": "owner_correction", "agent": self.agent, "submitted_semantic_key": correction.semantic_key,
            "messages": [{"session_id": context.session_key, "at_unix": now,
            "owner_text": owner_text, "source": context.ingress.source}]})
        .to_string();
        let agent = self.agent.clone();
        let result = tokio::task::spawn_blocking(move || {
            UserModelStore::shared(&data_dir)?.record_owner_correction(
                &agent,
                correction.kind,
                &correction.statement,
                &correction.semantic_key,
                &evidence,
                &context.session_key,
                now,
            )
        })
        .await?;
        Ok(match result {
            Ok(Some(_)) => ToolResult {
                success: true,
                output: text("tool-owner-correction-recorded").into(),
                error: None,
            },
            Ok(None) => denied("tool-owner-correction-pending"),
            Err(_) => denied("tool-owner-correction-store-error"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::review::{
        OwnerCorrectionContext, OwnerCorrectionResolver, UserMessageIngress,
    };
    use zeroclaw_memory::companion::{
        ApplicabilityContext, ReviewAction, project_applicable_heads,
    };

    fn args() -> serde_json::Value {
        json!({"kind": "preference", "statement": "Keep answers short.", "semantic_key": "answers.length"})
    }

    fn context(sender: &str) -> OwnerCorrectionContext {
        OwnerCorrectionContext {
            agent_alias: "nova".into(),
            session_key: "session-a".into(),
            ingress: UserMessageIngress {
                source: UserMessageSource::Channel {
                    sender_id: sender.into(),
                },
                text: "Please keep answers short.".into(),
            },
        }
    }

    fn tool() -> (
        tempfile::TempDir,
        Arc<parking_lot::RwLock<CompanionOwnerGate>>,
        NoteOwnerCorrectionTool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let owner = Arc::new(parking_lot::RwLock::new(CompanionOwnerGate {
            principal_id: "owner".into(),
            identities: vec![IngressIdentity::new("owner-channel")],
            trust_local: false,
        }));
        let live_owner = owner.clone();
        let path = dir.path().to_path_buf();
        let tool = NoteOwnerCorrectionTool::new("nova", move || {
            Some((live_owner.read().clone(), path.clone()))
        });
        (dir, owner, tool)
    }

    async fn call(
        tool: &NoteOwnerCorrectionTool,
        context: OwnerCorrectionContext,
        args: serde_json::Value,
    ) -> ToolResult {
        let resolver: OwnerCorrectionResolver = Arc::new(move || Some(context.clone()));
        zeroclaw_api::TOOL_LOOP_SESSION_KEY
            .scope(
                Some("session-a".into()),
                OWNER_CORRECTION_CONTEXT.scope(resolver, tool.execute(args)),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn note_owner_correction_requires_review_and_preserves_session_evidence() {
        let (dir, _, tool) = tool();
        assert!(call(&tool, context("owner-channel"), args()).await.success);
        let store = UserModelStore::shared(dir.path()).unwrap();
        let pending = store.list_pending_candidates().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].scope, "session:session-a");
        let evidence: serde_json::Value = serde_json::from_str(&pending[0].evidence).unwrap();
        assert_eq!(evidence["messages"][0]["session_id"], "session-a");
        assert_eq!(
            evidence["messages"][0]["owner_text"],
            "Please keep answers short."
        );
        assert_eq!(
            evidence["messages"][0]["source"]["sender_id"],
            "owner-channel"
        );
        assert!(store.active_heads(Some(u64::MAX / 2)).unwrap().is_empty());
        assert!(!dir.path().join("soul.db").exists());
        store
            .review_candidate(
                &pending[0].id,
                ReviewAction::Accept,
                "operator",
                None,
                None,
                10,
            )
            .unwrap();
        let heads = store.active_heads(Some(10)).unwrap();
        assert_eq!(heads[0].scope, "session:session-a");
        for (session, expected) in [("session-a", true), ("session-b", false)] {
            let projection = project_applicable_heads(
                heads.clone(),
                &ApplicabilityContext::new("nova", "telegram", session),
                1200,
            );
            assert_eq!(
                projection.prompt_section.contains("Keep answers short."),
                expected
            );
        }
    }

    #[tokio::test]
    async fn note_owner_correction_denies_forged_stale_and_wrong_turn_authority() {
        let (dir, owner, tool) = tool();
        assert!(!tool.execute(args()).await.unwrap().success);
        assert!(!call(&tool, context("stranger"), args()).await.success);
        let mut wrong_agent = context("owner-channel");
        wrong_agent.agent_alias = "worker".into();
        assert!(!call(&tool, wrong_agent, args()).await.success);
        let mut wrong_session = context("owner-channel");
        wrong_session.session_key = "session-b".into();
        assert!(!call(&tool, wrong_session, args()).await.success);
        let mut forged = args();
        forged["evidence"] = json!({"owner": true});
        assert!(!call(&tool, context("owner-channel"), forged).await.success);
        owner.write().identities.clear();
        assert!(!call(&tool, context("owner-channel"), args()).await.success);
        assert!(!dir.path().join("user_model.db").exists());
    }

    #[tokio::test]
    async fn note_owner_correction_is_bounded_and_not_inherited_by_workers() {
        assert!(
            zeroclaw_api::subagent_v1::SubAgentToolNameV1::parse("note_owner_correction").is_err()
        );
        let (dir, _, tool) = tool();
        let context = context("owner-channel");
        let resolver: OwnerCorrectionResolver = Arc::new(move || Some(context.clone()));
        OWNER_CORRECTION_CONTEXT
            .scope(resolver, async {
                assert!(
                    ::zeroclaw_spawn::spawn!(async {
                        OWNER_CORRECTION_CONTEXT.try_with(|_| ()).is_err()
                    })
                    .await
                    .unwrap()
                );
            })
            .await;
        assert!(OWNER_CORRECTION_CONTEXT.try_with(|_| ()).is_err());
        for i in 0..4 {
            let mut input = args();
            input["statement"] = json!(format!("Preference {i}."));
            assert_eq!(
                call(&tool, self::context("owner-channel"), input)
                    .await
                    .success,
                i < 3
            );
        }
        assert!(
            !call(&tool, self::context("owner-channel"), args())
                .await
                .success
        );
        assert_eq!(
            UserModelStore::shared(dir.path())
                .unwrap()
                .list_pending_candidates()
                .unwrap()
                .len(),
            3
        );
    }
}
