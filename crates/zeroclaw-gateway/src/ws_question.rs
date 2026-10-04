//! Ephemeral questions owned by the shared conversation. No durable task state.

use super::ws_conversation::FrameSink;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::oneshot, time::Instant};
use zeroclaw_config::schema::Config;

const MAX_PENDING: usize = 16;
const MAX_TEXT: usize = 4096;
const MAX_CHOICES: usize = 32;

struct Question {
    prompt: String,
    choices: Vec<String>,
    deadline: Instant,
    answer: oneshot::Sender<String>,
}

/// This registry creates the pending-question fact. Platform mappings only
/// reference its opaque id; answering it never grants a tool approval.
#[derive(Default)]
pub(crate) struct Questions(Mutex<HashMap<String, Question>>);

impl Questions {
    pub(crate) fn answer(&self, id: &str, text: &str) -> &'static str {
        let mut pending = self.0.lock();
        let Some(question) = pending.get(id) else {
            return "stale";
        };
        if question.deadline <= Instant::now() {
            pending.remove(id);
            return "stale";
        }
        let text = text.trim();
        if text.is_empty() || text.len() > MAX_TEXT {
            return "invalid";
        }
        let answer = if question.choices.is_empty() {
            text.to_owned()
        } else if let Some(choice) = question
            .choices
            .iter()
            .find(|choice| choice.as_str() == text)
        {
            choice.clone()
        } else if let Some(choice) = text
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| question.choices.get(n))
        {
            choice.clone()
        } else {
            return "invalid";
        };
        match pending.remove(id) {
            Some(question) => {
                if question.answer.send(answer).is_ok() {
                    "accepted"
                } else {
                    "stale"
                }
            }
            None => "stale",
        }
    }

    pub(crate) fn drain_if_offline(&self, frames: &FrameSink) {
        let mut pending = self.0.lock();
        if !frames.has_subscribers() {
            pending.clear();
        }
    }

    pub(crate) fn drain(&self) {
        self.0.lock().clear();
    }

    pub(crate) fn frames(&self, config: &Config) -> Vec<Value> {
        let now = Instant::now();
        self.0
            .lock()
            .iter()
            .filter(|(_, q)| q.deadline > now)
            .map(|(id, q)| question_frame(id, q, config))
            .collect()
    }
}

fn question_frame(id: &str, question: &Question, config: &Config) -> Value {
    let redact = |s: &str| super::ws::redact_frame_text(s, &config.security.leak_detection);
    json!({
        "type": "question", "request_id": id,
        "prompt": redact(&question.prompt),
        "choices": question.choices.iter().map(|s| redact(s)).collect::<Vec<_>>(),
        "timeout_secs": question.deadline.saturating_duration_since(Instant::now()).as_secs().saturating_add(1).min(300),
    })
}

#[derive(Clone)]
pub(crate) struct QuestionPort {
    pub(crate) questions: Arc<Questions>,
    pub(crate) frames: FrameSink,
    pub(crate) config: Arc<RwLock<Config>>,
}

// Also removes the question when the caller's future is cancelled/dropped.
struct PendingGuard {
    id: String,
    questions: Arc<Questions>,
    frames: FrameSink,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.questions.0.lock().remove(&self.id);
        self.frames
            .publish(&json!({"type":"question_closed", "request_id": self.id}));
    }
}

impl QuestionPort {
    pub(crate) async fn ask(
        &self,
        prompt: &str,
        choices: &[String],
        timeout: Duration,
    ) -> anyhow::Result<Option<String>> {
        anyhow::ensure!(
            !prompt.trim().is_empty() && prompt.len() <= MAX_TEXT,
            "question.invalid_prompt"
        );
        anyhow::ensure!(
            prompt
                .len()
                .saturating_add(choices.iter().map(String::len).sum::<usize>())
                <= 3000,
            "question.display_limit"
        );
        anyhow::ensure!(
            choices.len() <= MAX_CHOICES
                && choices
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= MAX_TEXT),
            "question.invalid_choices"
        );
        anyhow::ensure!(
            (Duration::from_secs(1)..=Duration::from_secs(300)).contains(&timeout),
            "question.invalid_timeout"
        );
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let deadline = Instant::now() + timeout;
        {
            let mut pending = self.questions.0.lock();
            anyhow::ensure!(pending.len() < MAX_PENDING, "question.capacity");
            if !self.frames.has_subscribers() {
                return Ok(None);
            }
            let question = Question {
                prompt: prompt.to_owned(),
                choices: choices.to_vec(),
                deadline,
                answer: tx,
            };
            // Read live policy at publication, never snapshot it in a handle.
            let frame = question_frame(&id, &question, &self.config.read());
            pending.insert(id.clone(), question);
            self.frames.publish(&frame);
        }
        let _guard = PendingGuard {
            id,
            questions: self.questions.clone(),
            frames: self.frames.clone(),
        };
        Ok(tokio::time::timeout_at(deadline, rx)
            .await
            .ok()
            .and_then(Result::ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws_conversation::{ConversationHub, Submitted, Subscription};
    use zeroclaw_api::tool::Tool;

    async fn setup() -> (Subscription<()>, QuestionPort) {
        let hub = Arc::new(ConversationHub::default());
        let config = Arc::new(RwLock::new(Config::default()));
        let (sub, port) = hub
            .attach("session", |seed| {
                let port = QuestionPort {
                    questions: seed.questions,
                    frames: seed.frames,
                    config: config.clone(),
                };
                async { Ok::<_, ()>(((), port)) }
            })
            .await
            .unwrap();
        (sub, port.unwrap())
    }

    async fn next_question(sub: &mut Subscription<()>) -> Value {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), sub.frames.recv())
                .await
                .unwrap()
                .unwrap();
            let frame: Value = serde_json::from_str(&frame).unwrap();
            if frame["type"] == "question" {
                return frame;
            }
        }
    }

    #[tokio::test]
    async fn real_ask_user_uses_correlated_channel_without_legacy_listen() {
        let (mut sub, port) = setup().await;
        let (events, _) = tokio::sync::mpsc::channel(8);
        let channel = Arc::new(
            crate::ws_approval::WsApprovalChannel::new(
                events,
                crate::ws_approval::new_pending_approvals(),
                Duration::from_secs(5),
            )
            .with_questions(port.clone()),
        );
        let channels: zeroclaw_tools::ask_user::ChannelMapHandle =
            Arc::new(RwLock::new(HashMap::new()));
        channels.write().insert("wss".into(), channel);
        let tool = zeroclaw_tools::ask_user::AskUserTool::new(
            Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
            channels,
        );
        let task = zeroclaw_spawn::spawn!(async move {
            tool.execute(json!({"channel":"wss", "question":"Which?", "choices":["alpha", "beta"], "timeout_secs":5})).await.unwrap()
        });
        let question = next_question(&mut sub).await;
        let id = question["request_id"].as_str().unwrap();
        assert_eq!(port.questions.answer(id, "other"), "invalid");
        assert_eq!(port.questions.answer(id, "2"), "accepted");
        assert_eq!(port.questions.answer(id, "1"), "stale");
        let result = task.await.unwrap();
        assert!(result.success, "{result:?}");
        assert_eq!(result.output.as_str(), "beta");
        assert!(port.questions.0.lock().is_empty());
    }

    #[tokio::test]
    async fn free_text_timeout_and_dropped_future_remove_pending_requests() {
        let (mut sub, port) = setup().await;
        let p = port.clone();
        let task = zeroclaw_spawn::spawn!(async move {
            p.ask("Name?", &[], Duration::from_secs(2)).await.unwrap()
        });
        let q = next_question(&mut sub).await;
        assert_eq!(
            port.questions
                .answer(q["request_id"].as_str().unwrap(), "  hello  "),
            "accepted"
        );
        assert_eq!(task.await.unwrap().as_deref(), Some("hello"));
        let p = port.clone();
        let task = zeroclaw_spawn::spawn!(async move {
            p.ask("Expire?", &[], Duration::from_secs(1)).await.unwrap()
        });
        let q = next_question(&mut sub).await;
        assert!(task.await.unwrap().is_none());
        assert_eq!(
            port.questions
                .answer(q["request_id"].as_str().unwrap(), "late"),
            "stale"
        );
        let p = port.clone();
        let task = zeroclaw_spawn::spawn!(async move {
            p.ask("Drop?", &[], Duration::from_secs(5)).await.unwrap()
        });
        next_question(&mut sub).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(port.questions.0.lock().is_empty());
    }

    #[tokio::test]
    async fn cancel_and_last_disconnect_release_waiters_without_cancelling_the_turn() {
        for cancel in [false, true] {
            let (mut sub, port) = setup().await;
            let Submitted::Start(claim) = sub.conversation.submit("start".into()) else {
                panic!()
            };
            let conv = sub.conversation.clone();
            let p = port.clone();
            let task = zeroclaw_spawn::spawn!(async move {
                p.ask("Wait?", &[], Duration::from_secs(5)).await.unwrap()
            });
            next_question(&mut sub).await;
            if cancel {
                assert!(conv.cancel_current());
            } else {
                drop(sub);
            }
            assert!(
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
            assert_eq!(claim.cancel.is_cancelled(), cancel);
            assert!(port.questions.0.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn replay_keeps_id_and_reads_current_redaction_policy() {
        let (mut sub, port) = setup().await;
        let p = port.clone();
        let token = format!("zc_{}", "abcdef12".repeat(8));
        let prompt = format!("Choose {token}");
        port.config.write().security.leak_detection.enabled = false;
        let task = zeroclaw_spawn::spawn!(async move {
            p.ask(&prompt, &[], Duration::from_secs(5)).await.unwrap()
        });
        let first = next_question(&mut sub).await;
        assert!(first["prompt"].as_str().unwrap().contains(&token));
        port.config.write().security.leak_detection.enabled = true;
        let replay = port.questions.frames(&port.config.read());
        assert_eq!(replay[0]["request_id"], first["request_id"]);
        assert!(!replay[0]["prompt"].as_str().unwrap().contains(&token));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn no_subscribers_and_invalid_bounds_never_create_pending_state() {
        let (sub, port) = setup().await;
        assert!(port.ask("large", &[], Duration::ZERO).await.is_err());
        assert!(
            port.ask(&"a".repeat(3001), &[], Duration::from_secs(1))
                .await
                .is_err()
        );
        drop(sub);
        assert!(
            port.ask("Where?", &[], Duration::from_secs(2))
                .await
                .unwrap()
                .is_none()
        );
        assert!(port.questions.0.lock().is_empty());
    }
}
