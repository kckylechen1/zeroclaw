//! Thin L2 tools. Tachi owns execution truth; the existing session database
//! owns only local request admission and its canonical dispatch reference.
//! No identity, memory, conversation history, or execution authority is sent.

use crate::tachi_staff::{StaffRefs, StaffingReason, TachiStaffClient, TachiStaffError};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use zeroclaw_api::delegation_request::DelegationRequestClaim;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::policy::ToolOperation;
use zeroclaw_config::tachi::TachiConfig;
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;

/// The runtime rechecks live agent/card policy and returns ephemeral config.
type DelegationResolver =
    Arc<dyn Fn(&str, ToolOperation) -> Result<(TachiConfig, PathBuf), String> + Send + Sync>;

/// Assemble the L2 tools against one runtime-owned authorization/config resolver.
/// This is the same control surface for any caller; it accepts no raw process authority.
pub fn tools(
    agent_alias: &str,
    resolve: impl Fn(&str, ToolOperation) -> Result<(TachiConfig, PathBuf), String>
    + Send
    + Sync
    + 'static,
) -> Vec<Arc<dyn Tool>> {
    let service = Arc::new(TachiDelegation {
        agent_alias: agent_alias.to_string(),
        resolve: Arc::new(resolve),
    });
    DelegationAction::ALL
        .into_iter()
        .map(|action| {
            Arc::new(TachiDelegationTool {
                service: service.clone(),
                action,
            }) as Arc<dyn Tool>
        })
        .collect()
}

/// The service shared by the registered tools. It holds transport policy
/// resolvers, not a second task registry. DB uniqueness also covers other
/// tool registries/processes and a restart between admission and the response.
struct TachiDelegation {
    agent_alias: String,
    resolve: DelegationResolver,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DelegationAction {
    Start,
    Status,
    Result,
    Cancel,
    Watch,
}

impl DelegationAction {
    pub const ALL: [Self; 5] = [
        Self::Start,
        Self::Status,
        Self::Result,
        Self::Cancel,
        Self::Watch,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "tachi_start",
            Self::Status => "tachi_status",
            Self::Result => "tachi_result",
            Self::Cancel => "tachi_cancel",
            Self::Watch => "tachi_watch",
        }
    }
    fn operation(self) -> ToolOperation {
        match self {
            Self::Start | Self::Cancel => ToolOperation::Act,
            _ => ToolOperation::Read,
        }
    }
    fn description(self) -> &'static str {
        static STRINGS: OnceLock<[String; 5]> = OnceLock::new();
        let strings = STRINGS.get_or_init(|| {
            Self::ALL.map(|action| text(&format!("tool-{}", action.name().replace('_', "-"))))
        });
        &strings[self as usize]
    }
}

struct TachiDelegationTool {
    service: Arc<TachiDelegation>,
    action: DelegationAction,
}

zeroclaw_api::tool_attribution!(
    TachiDelegationTool,
    zeroclaw_api::attribution::ToolKind::Plugin
);

fn text(key: &str) -> String {
    crate::i18n::get_required_tool_string(key)
}

fn failure(code: &str, detail: &str) -> ToolResult {
    let detail = zeroclaw_providers::sanitize_api_error(detail);
    let message =
        crate::i18n::get_required_tool_string_with_args("tool-tachi-error", &[("detail", &detail)]);
    ToolResult::partial(json!({"code": code}), message)
}

fn staff_failure(error: TachiStaffError) -> ToolResult {
    let code = match &error {
        TachiStaffError::Unavailable(_) => "unavailable",
        TachiStaffError::SubmissionUnknown(_) => "submission_unknown",
        TachiStaffError::Refused(_) => "refused",
        TachiStaffError::UnknownHarness(_) => "unknown_harness",
        TachiStaffError::CancelUnsupported { .. } => "cancel_unsupported",
        TachiStaffError::StaleRevision { .. } => "stale_revision",
        TachiStaffError::Protocol(_) => "protocol_error",
    };
    failure(code, &error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    request_id: String,
    harness: String,
    task: String,
    staffing_reason: StaffingReason,
    issue_ref: Option<String>,
    pr_ref: Option<String>,
    flow_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    request_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    request_id: String,
    expected_status_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchArgs {
    request_id: String,
    max_wait_secs: u64,
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':'))
}
fn hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

#[async_trait]
impl Tool for TachiDelegationTool {
    fn name(&self) -> &str {
        self.action.name()
    }
    fn description(&self) -> &str {
        self.action.description()
    }
    fn parameters_schema(&self) -> Value {
        let mut properties = json!({"request_id": {"type":"string", "description":text("tool-tachi-param-request-id"), "minLength":1, "maxLength":128}});
        let mut required = vec!["request_id"];
        match self.action {
            DelegationAction::Start => {
                properties["harness"] =
                    json!({"type":"string", "description":text("tool-tachi-param-harness")});
                properties["task"] = json!({"type":"string", "description":text("tool-tachi-param-task"), "minLength":1, "maxLength":16384});
                properties["staffing_reason"] = json!({"type":"string", "description":text("tool-tachi-param-reason"), "enum":StaffingReason::ALL.map(StaffingReason::as_str)});
                for key in ["issue_ref", "pr_ref", "flow_id"] {
                    properties[key] =
                        json!({"type":"string", "description":text("tool-tachi-param-ref")});
                }
                required.extend(["harness", "task", "staffing_reason"]);
            }
            DelegationAction::Cancel => {
                properties["expected_status_revision"] = json!({"type":"integer", "minimum":0, "description":text("tool-tachi-param-revision")});
                required.push("expected_status_revision");
            }
            DelegationAction::Watch => {
                properties["max_wait_secs"] = json!({"type":"integer", "minimum":1, "maximum":30, "description":text("tool-tachi-param-wait")});
                required.push("max_wait_secs");
            }
            _ => {}
        }
        json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let (tachi, data_dir) = match (self.service.resolve)(self.name(), self.action.operation()) {
            Ok(value) => value,
            Err(detail) => return Ok(failure("denied", &detail)),
        };
        let client = match TachiStaffClient::from_config(&tachi, &self.service.agent_alias) {
            Ok(client) => client,
            Err(error) => return Ok(staff_failure(error)),
        };
        // A route/actor/project change cannot read or cancel an old binding on
        // a different daemon. This scope is provenance of the local request,
        // not a snapshot of live authorization.
        let scope = format!(
            "{}:{}",
            self.service.agent_alias,
            hash(&json!({
                "endpoint": tachi.endpoint.trim(),
                "identity": tachi.resolved_agent_identity(&self.service.agent_alias),
                "project":tachi.project.as_deref().map(str::trim),
            }))
        );
        let start: Option<StartArgs> = if self.action == DelegationAction::Start {
            match serde_json::from_value(args.clone()) {
                Ok(value) => Some(value),
                Err(error) => return Ok(failure("invalid_arguments", &error.to_string())),
            }
        } else {
            None
        };
        let request_id = match self.action {
            DelegationAction::Start => start.as_ref().map(|a| a.request_id.clone()),
            DelegationAction::Cancel => serde_json::from_value::<CancelArgs>(args.clone())
                .ok()
                .map(|a| a.request_id),
            DelegationAction::Watch => serde_json::from_value::<WatchArgs>(args.clone())
                .ok()
                .filter(|a| (1..=30).contains(&a.max_wait_secs))
                .map(|a| a.request_id),
            _ => serde_json::from_value::<ReadArgs>(args.clone())
                .ok()
                .map(|a| a.request_id),
        };
        let Some(request_id) = request_id.filter(|id| valid_request_id(id)) else {
            return Ok(failure("invalid_arguments", &text("tool-tachi-invalid")));
        };
        if let Some(start) = &start {
            if start.task.trim().is_empty() || start.task.len() > 16384 {
                return Ok(failure("invalid_arguments", &text("tool-tachi-invalid")));
            }
            if let Err(error) = client.profile_for(&start.harness) {
                return Ok(staff_failure(error));
            }
            for value in [start.task.as_str(), request_id.as_str()]
                .into_iter()
                .chain(start.issue_ref.as_deref())
                .chain(start.pr_ref.as_deref())
                .chain(start.flow_id.as_deref())
            {
                if let Err(category) = crate::tachi_admission::scan_text(value) {
                    return Ok(failure("forbidden_content", &category.to_string()));
                }
            }
        }
        let db = match tokio::task::spawn_blocking(move || SqliteSessionBackend::new(&data_dir))
            .await?
        {
            Ok(db) => Arc::new(db),
            Err(error) => return Ok(failure("storage_error", &error.to_string())),
        };
        if let Some(start) = start {
            let digest =
                hash(&json!({"profile":client.profile_for(&start.harness)?, "arguments":args}));
            let claim = {
                let (db, scope, request_id, digest) = (
                    db.clone(),
                    scope.clone(),
                    request_id.clone(),
                    digest.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    db.claim_delegation_request(&scope, &request_id, &digest)
                })
                .await?
            };
            match claim {
                Ok(DelegationRequestClaim::Created) => {}
                Ok(DelegationRequestClaim::Existing {
                    dispatch_id: Some(id),
                }) => {
                    return Ok(ToolResult::ok(
                        json!({"request_id":request_id,"dispatch_id":id,"accepted":true,"replayed":true}),
                    ));
                }
                Ok(DelegationRequestClaim::Existing { dispatch_id: None }) => {
                    return Ok(failure(
                        "submission_unresolved",
                        &text("tool-tachi-unresolved"),
                    ));
                }
                Ok(DelegationRequestClaim::Conflict) => {
                    return Ok(failure("request_conflict", &text("tool-tachi-conflict")));
                }
                Err(error) => return Ok(failure("storage_error", &error.to_string())),
            }
            let refs = StaffRefs {
                issue_ref: start.issue_ref,
                pr_ref: start.pr_ref,
                flow_id: start.flow_id,
            };
            let receipt = match client
                .start(&start.harness, &start.task, start.staffing_reason, &refs)
                .await
            {
                Ok(receipt) => receipt,
                Err(error @ TachiStaffError::Unavailable(_)) => {
                    // start's Unavailable contract proves tools/call was never
                    // sent. Other failures never release a durable claim.
                    let release = tokio::task::spawn_blocking(move || {
                        db.release_unsent_delegation_request(&scope, &request_id, &digest)
                    })
                    .await?;
                    if let Err(storage) = release {
                        return Ok(failure("storage_error", &storage.to_string()));
                    }
                    return Ok(staff_failure(error));
                }
                Err(error) => return Ok(staff_failure(error)),
            };
            let id = receipt.dispatch_id.clone();
            let output_request_id = request_id.clone();
            let stored = tokio::task::spawn_blocking(move || {
                db.bind_delegation_request(&scope, &request_id, &digest, &id)
            })
            .await?;
            if let Err(error) = stored {
                return Ok(ToolResult::partial(
                    json!({"code":"binding_write_failed", "request_id":output_request_id, "receipt":receipt}),
                    crate::i18n::get_required_tool_string_with_args(
                        "tool-tachi-binding-failed",
                        &[(
                            "detail",
                            &zeroclaw_providers::sanitize_api_error(&error.to_string()),
                        )],
                    ),
                ));
            }
            return Ok(ToolResult::ok(
                json!({"request_id":output_request_id,"accepted":true,"receipt":receipt,"replayed":false}),
            ));
        }
        let binding = {
            let (request_id, scope) = (request_id.clone(), scope.clone());
            tokio::task::spawn_blocking(move || db.read_delegation_request(&scope, &request_id))
                .await?
        };
        let dispatch_id = match binding {
            Ok(Some(binding)) => match binding.dispatch_id {
                Some(id) => id,
                None => {
                    return Ok(failure(
                        "submission_unresolved",
                        &text("tool-tachi-unresolved"),
                    ));
                }
            },
            Ok(None) => return Ok(failure("unknown_request", &text("tool-tachi-unknown"))),
            Err(error) => return Ok(failure("storage_error", &error.to_string())),
        };
        let output = match self.action {
            DelegationAction::Status => client.status(&dispatch_id).await.map(|status| json!({"request_id":request_id,"status":status})),
            DelegationAction::Result => client.result(&dispatch_id).await.map(|mut result| {
                result.body = result.body.map(|body| zeroclaw_providers::scrub_secret_patterns(&body));
                json!({"request_id":request_id,"result":result,"trust":"untrusted_external_report","accepted_by_body":false})
            }),
            DelegationAction::Cancel => {
                let cancel: CancelArgs = serde_json::from_value(args)?;
                client.cancel(&dispatch_id, cancel.expected_status_revision).await.map(|receipt| json!({"request_id":request_id,"dispatch_id":dispatch_id,"receipt":receipt}))
            }
            DelegationAction::Watch => {
                let watch: WatchArgs = serde_json::from_value(args)?;
                client.wait_until_terminal(&dispatch_id, Duration::from_secs(watch.max_wait_secs)).await.map(|status| json!({"request_id":request_id,"terminal":status.is_terminal(),"status":status,"watch":"bounded_polling"}))
            }
            DelegationAction::Start => Err(TachiStaffError::Protocol("start reached read path".to_string())),
        };
        Ok(match output {
            Ok(value) => ToolResult::ok(value),
            Err(error) => staff_failure(error),
        })
    }
}
