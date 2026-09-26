//! `notify`: send a proactive message through a channel bridge.
//!
//! The message is queued in the bridge outbox (`[gateway.bridges.<name>]`)
//! and delivered the next time that bridge's control socket is connected.
//! It is the one proactive-message tool for bridged channels; registered
//! only when at least one bridge is configured.

use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::Config;

pub struct NotifyTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl NotifyTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn bridge_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.config.gateway.bridges.keys().cloned().collect();
        names.sort();
        names
    }
}

fn failure(error: String) -> ToolResult {
    ToolResult {
        success: false,
        output: ToolOutput::default(),
        error: Some(error),
    }
}

fn required<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[async_trait]
impl Tool for NotifyTool {
    fn name(&self) -> &str {
        "notify"
    }

    fn description(&self) -> &str {
        "Send a message to the owner through a channel bridge (for example Telegram), \
         outside the current conversation. The message is queued and delivered when \
         the bridge is connected. Use it for proactive updates, not for replying in \
         the current chat."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "bridge": {
                    "type": "string",
                    "enum": self.bridge_names(),
                    "description": "Configured bridge to send through"
                },
                "to": {
                    "type": "string",
                    "description": "Recipient on the bridge's platform, e.g. a Telegram chat id"
                },
                "text": {
                    "type": "string",
                    "description": "Message text"
                }
            },
            "required": ["bridge", "to", "text"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let (Some(bridge), Some(to), Some(text)) = (
            required(&args, "bridge"),
            required(&args, "to"),
            required(&args, "text"),
        ) else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                "notify: bridge, to and text are required"
            );
            return Ok(failure(
                "'bridge', 'to' and 'text' are all required".to_string(),
            ));
        };
        if !self.config.gateway.bridges.contains_key(bridge) {
            return Ok(failure(format!(
                "unknown bridge {bridge:?}; configured bridges: {:?}",
                self.bridge_names()
            )));
        }
        if !self.security.can_act() {
            return Ok(failure(
                "Security policy: read-only mode, cannot perform 'notify'".to_string(),
            ));
        }
        if self.security.is_rate_limited() || !self.security.record_action() {
            return Ok(failure(
                "Rate limit exceeded: action budget exhausted".to_string(),
            ));
        }
        match crate::cron::scheduler::enqueue_for_bridge(&self.config, bridge, to, None, text) {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("queued for {bridge} to {to}").into(),
                error: None,
            }),
            Err(e) => Ok(failure(format!("could not queue the message: {e:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;
    use zeroclaw_config::schema::GatewayBridgeConfig;

    fn tool(dir: &tempfile::TempDir, autonomy: AutonomyLevel) -> NotifyTool {
        let mut config = Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        config
            .gateway
            .bridges
            .insert("telegram".into(), GatewayBridgeConfig::default());
        let security = SecurityPolicy {
            autonomy,
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        };
        NotifyTool::new(Arc::new(config), Arc::new(security))
    }

    fn queued(dir: &tempfile::TempDir) -> Vec<String> {
        zeroclaw_infra::bridge_outbox::BridgeOutbox::shared(dir.path())
            .unwrap()
            .pending("telegram", 0, 10)
            .unwrap()
            .into_iter()
            .map(|item| format!("{}:{}", item.to, item.content))
            .collect()
    }

    #[tokio::test]
    async fn notify_queues_for_a_configured_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(&dir, AutonomyLevel::Supervised);
        assert_eq!(
            tool.parameters_schema()["properties"]["bridge"]["enum"],
            json!(["telegram"])
        );
        let result = tool
            .execute(json!({"bridge": "telegram", "to": "42", "text": "build is green"}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(queued(&dir), ["42:build is green"]);
    }

    #[tokio::test]
    async fn notify_refuses_unknown_bridges_missing_fields_and_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(&dir, AutonomyLevel::Supervised);
        let unknown = tool
            .execute(json!({"bridge": "slack", "to": "42", "text": "x"}))
            .await
            .unwrap();
        assert!(!unknown.success);
        let missing = tool
            .execute(json!({"bridge": "telegram", "to": "42"}))
            .await
            .unwrap();
        assert!(!missing.success);

        let read_only = tool_read_only(&dir);
        let refused = read_only
            .execute(json!({"bridge": "telegram", "to": "42", "text": "x"}))
            .await
            .unwrap();
        assert!(!refused.success);
        assert!(queued(&dir).is_empty());
    }

    fn tool_read_only(dir: &tempfile::TempDir) -> NotifyTool {
        tool(dir, AutonomyLevel::ReadOnly)
    }
}
