//! Versioned edge control over the body's existing delegation admission.
//! These messages project Tachi receipts; they do not own an execution state.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tachi_staff::StaffingReason;

pub const VERSION: u8 = 1;
pub const MAX_RECENT: u8 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationOperation {
    Start,
    Status,
    Result,
    Cancel,
    List,
}

impl DelegationOperation {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "status" => Some(Self::Status),
            "result" => Some(Self::Result),
            "cancel" => Some(Self::Cancel),
            "list" => Some(Self::List),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Status => "status",
            Self::Result => "result",
            Self::Cancel => "cancel",
            Self::List => "list",
        }
    }

    /// The bridge grant and agent tool permission both use this exact name.
    pub fn tool_name(self) -> &'static str {
        match self {
            Self::Start => "tachi_start",
            Self::Status | Self::List => "tachi_status",
            Self::Result => "tachi_result",
            Self::Cancel => "tachi_cancel",
        }
    }
}

/// Flat, duplicate/unknown-field-rejecting HTTP request. Fields irrelevant to
/// an operation are refused by `arguments`, including raw execution authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationControlRequest {
    pub version: u8,
    pub session_id: String,
    pub request_id: Option<String>,
    pub harness: Option<String>,
    pub task: Option<String>,
    pub staffing_reason: Option<StaffingReason>,
    pub issue_ref: Option<String>,
    pub pr_ref: Option<String>,
    pub flow_id: Option<String>,
    pub expected_status_revision: Option<u64>,
    pub limit: Option<u8>,
}

impl DelegationControlRequest {
    /// Convert only the operation's admitted fields to the existing tool call.
    /// `None` is a protocol refusal, never a best-effort partial request.
    pub fn arguments(&self, operation: DelegationOperation) -> Option<Value> {
        if self.version != VERSION
            || self.session_id.is_empty()
            || self.session_id.len() > 128
            || crate::session_keys::sanitize_session_key(&self.session_id) != self.session_id
        {
            return None;
        }
        let has_start = self.harness.is_some()
            || self.task.is_some()
            || self.staffing_reason.is_some()
            || self.issue_ref.is_some()
            || self.pr_ref.is_some()
            || self.flow_id.is_some();
        if operation == DelegationOperation::List {
            let limit = self.limit.unwrap_or(MAX_RECENT);
            return (!has_start
                && self.request_id.is_none()
                && self.expected_status_revision.is_none()
                && (1..=MAX_RECENT).contains(&limit))
            .then(|| json!({"limit": limit}));
        }
        let id = self.request_id.as_deref().filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })?;
        if self.limit.is_some() {
            return None;
        }
        let mut arguments = json!({"request_id": id});
        if operation == DelegationOperation::Start {
            if self.expected_status_revision.is_some() {
                return None;
            }
            let harness = self
                .harness
                .as_deref()
                .filter(|v| !v.is_empty() && v.len() <= 128)?;
            let task = self
                .task
                .as_deref()
                .filter(|v| !v.trim().is_empty() && v.len() <= 16384)?;
            let reason = self.staffing_reason?;
            arguments["harness"] = json!(harness);
            arguments["task"] = json!(task);
            arguments["staffing_reason"] = json!(reason);
            for (key, value) in [
                ("issue_ref", &self.issue_ref),
                ("pr_ref", &self.pr_ref),
                ("flow_id", &self.flow_id),
            ] {
                if let Some(value) = value {
                    if value.len() > 1024 {
                        return None;
                    }
                    arguments[key] = json!(value);
                }
            }
        } else {
            if has_start {
                return None;
            }
            if operation == DelegationOperation::Cancel {
                arguments["expected_status_revision"] = json!(self.expected_status_revision?);
            } else if self.expected_status_revision.is_some() {
                return None;
            }
        }
        Some(arguments)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationControlResponse {
    pub version: u8,
    pub operation: String,
    pub request_id: Option<String>,
    pub success: bool,
    /// The existing structured tool result, not a model's prose claim.
    pub output: Value,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> DelegationControlRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn operation_fields_and_bounds_are_not_interchangeable() {
        let read = request(json!({"version":1,"session_id":"voice","request_id":"request-1"}));
        assert_eq!(
            read.arguments(DelegationOperation::Status),
            Some(json!({"request_id":"request-1"}))
        );
        assert!(read.arguments(DelegationOperation::Start).is_none());
        assert!(read.arguments(DelegationOperation::Cancel).is_none());
        assert!(read.arguments(DelegationOperation::List).is_none());
        let cancel = request(
            json!({"version":1,"session_id":"voice","request_id":"request-1","expected_status_revision":7}),
        );
        assert_eq!(
            cancel.arguments(DelegationOperation::Cancel).unwrap()["expected_status_revision"],
            7
        );
        assert!(cancel.arguments(DelegationOperation::Status).is_none());
        for limit in [0, 21] {
            assert!(
                request(json!({"version":1,"session_id":"voice","limit":limit}))
                    .arguments(DelegationOperation::List)
                    .is_none()
            );
        }
        assert_eq!(
            request(json!({"version":1,"session_id":"voice"})).arguments(DelegationOperation::List),
            Some(json!({"limit":20}))
        );
        assert!(
            request(json!({"version":2,"session_id":"voice","request_id":"request-1"}))
                .arguments(DelegationOperation::Status)
                .is_none()
        );
    }

    #[test]
    fn unknown_authority_duplicate_fields_and_invalid_binding_are_refused() {
        for extra in [
            "command",
            "cwd",
            "model",
            "agent",
            "dispatch_id",
            "approved",
        ] {
            let mut value = json!({"version":1,"session_id":"voice","request_id":"r"});
            value[extra] = json!("injected");
            assert!(serde_json::from_value::<DelegationControlRequest>(value).is_err());
        }
        assert!(
            serde_json::from_str::<DelegationControlRequest>(
                r#"{"version":1,"session_id":"one","session_id":"two"}"#
            )
            .is_err()
        );
        assert!(
            request(json!({"version":1,"session_id":"../other","request_id":"r"}))
                .arguments(DelegationOperation::Status)
                .is_none()
        );
    }
}
