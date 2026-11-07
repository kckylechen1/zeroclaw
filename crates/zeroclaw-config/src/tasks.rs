//! Task-list reminder configuration (`[tasks]`, #568): the owner-selected
//! destination armed reminder occurrences copy at creation. The occurrence
//! stores the copy; the CURRENT config decides whether delivery is still
//! permitted (see the reminder delivery gate) — a later target change never
//! rewrites an existing occurrence's destination.

use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// Reminder destination (`[tasks.reminder_target]`); same field rules as the
/// review/delegation notification targets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "tasks.reminder_target"]
pub struct ReminderTargetConfig {
    pub bridge: String,
    pub recipient: String,
    pub thread_id: Option<String>,
}

impl ReminderTargetConfig {
    /// Mirrors the review-notification target's field rules.
    pub fn validate(&self) -> Result<(), String> {
        for (field, value) in [
            ("bridge", self.bridge.as_str()),
            ("recipient", self.recipient.as_str()),
        ]
        .into_iter()
        .chain(self.thread_id.as_deref().map(|v| ("thread_id", v)))
        {
            if value.is_empty()
                || value.trim() != value
                || value.len() > 256
                || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "tasks.reminder_target.{field} must be nonempty, trimmed, at most 256 bytes and free of control characters"
                ));
            }
        }
        Ok(())
    }
}

/// `[tasks]` — task-list behavior. Absent by default: without a valid
/// `reminder_target`, arming a reminder is refused with a clear error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "tasks"]
pub struct TasksConfig {
    #[nested]
    pub reminder_target: Option<ReminderTargetConfig>,
}

impl TasksConfig {
    /// Whether the section carries any setting.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.reminder_target.is_none()
    }
}
