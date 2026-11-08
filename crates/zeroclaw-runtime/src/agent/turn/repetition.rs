//! Per-turn evidence from completed dispatches. Only hashes survive a round.

use crate::agent::tool_execution::{CompletedToolExecution, ToolExecutionOutcome};
use regex::Regex;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
use zeroclaw_tool_call_parser::ParsedToolCall;

#[derive(Default)]
pub(crate) struct RepetitionGuard {
    last: Option<(Signature, bool)>,
    repeats: usize,
    warned_failure: Option<Signature>,
    success_advised: bool,
    exhausted: bool,
}

#[derive(Default)]
pub(super) struct RepetitionDisposition {
    pub(super) message_key: Option<&'static str>,
    pub(super) close: bool,
}

impl RepetitionGuard {
    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    pub(super) fn recovery_call_hash(&self) -> Option<u64> {
        self.warned_failure.map(|warned| warned.call)
    }

    pub(super) fn observe_round(
        &mut self,
        calls: &[ParsedToolCall],
        outcomes: &[CompletedToolExecution],
        ignored: &HashSet<&str>,
        threshold: usize,
    ) -> RepetitionDisposition {
        let evidence: Vec<_> = calls
            .iter()
            .zip(outcomes)
            .filter_map(|(call, completed)| {
                let name = completed.executed_tool_name.as_deref()?;
                if ignored.contains(name) {
                    return None;
                }
                Some((
                    signature(name, &call.arguments, &completed.outcome),
                    completed.outcome.success,
                ))
            })
            .collect();

        // A warning raised in this batch cannot spend the recovery attempt.
        // Inspect the whole later batch before closing, so another completed
        // call that changed the approach or succeeded can recover the turn.
        if let Some(warned) = self.warned_failure {
            if evidence
                .iter()
                .any(|&(hash, success)| success || hash != warned)
            {
                self.warned_failure = None;
            } else if evidence.iter().any(|&(hash, _)| hash == warned) {
                self.exhausted = true;
                return RepetitionDisposition {
                    message_key: Some("turn-repeated-failure-exhausted"),
                    close: true,
                };
            }
        }

        let mut disposition = RepetitionDisposition::default();
        for (hash, success) in evidence {
            if self.last == Some((hash, success)) {
                self.repeats = self.repeats.saturating_add(1);
            } else {
                self.last = Some((hash, success));
                self.repeats = 1;
                self.warned_failure = None;
            }
            if self.repeats < threshold.max(1) {
                continue;
            }
            if success {
                if !self.success_advised {
                    self.success_advised = true;
                    disposition.message_key = Some("turn-repeated-success-advisory");
                }
            } else if self.warned_failure != Some(hash) {
                self.warned_failure = Some(hash);
                disposition.message_key = Some("turn-repeated-failure-recovery");
            }
        }
        // A changed call later in the warning batch invalidates that warning.
        if disposition.message_key == Some("turn-repeated-failure-recovery")
            && self.warned_failure.is_none()
        {
            disposition.message_key = None;
        }
        disposition
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Signature {
    call: u64,
    result: u64,
}

pub(super) fn call_signature(name: &str, arguments: &serde_json::Value) -> u64 {
    let mut args = arguments.clone();
    if crate::agent::is_runtime_approved_arg_tool(name)
        && let Some(args) = args.as_object_mut()
    {
        args.remove("approved");
    }
    let args = zeroclaw_tool_call_parser::canonicalize_json_for_tool_signature(&args);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    args.to_string().hash(&mut hasher);
    hasher.finish()
}

static FAILURE_TIMESTAMP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2}|\s+UTC)?\b",
    )
    .unwrap()
});

static FAILURE_PROCESS_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(pid|ppid|tid)(\s*[:=]\s*|\s+#?)\d+\b").unwrap());

static FAILURE_REQUEST_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(request[_-]?id|req[_-]?id|trace[_-]?id|span[_-]?id|correlation[_-]?id|cf-ray)(["']?\s*[:=]\s*["']?)[A-Za-z0-9._-]+"#,
    )
    .unwrap()
});

static FAILURE_DURATION_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(elapsed(?:\s+time)?|duration|took|completed\s+in|failed\s+in)(\s*[:=]?\s*)\d+(?:\.\d+)?\s*(?:ns|µs|us|ms|s|sec|secs|seconds)\b",
    )
    .unwrap()
});

static FAILURE_BRACKETED_SUBSEC_DURATION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([(\[])\d+(?:\.\d+)?\s*(?:ns|µs|us|ms)([)\]])").unwrap());

static FAILURE_ATTEMPT_COUNTER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(attempt|retry)(\s+#?\s*|\s*[:=]\s*)\d+(?:\s*(?:/|of)\s*\d+)?\b").unwrap()
});

/// Narrowly normalizes volatile observation telemetry inside failed tool outputs
/// so repeated identical failures still converge when wrappers emit per-call
/// timestamps, PIDs, request/trace IDs, elapsed durations, or attempt counters.
///
/// Never runs on successful outputs and never strips exit codes, HTTP status
/// codes, line numbers, paths, configured timeout windows, or resource IDs.
fn normalize_failure_observation_for_signature(scrubbed: &str) -> String {
    let without_ts = FAILURE_TIMESTAMP_REGEX.replace_all(scrubbed, "<timestamp>");
    let without_pid = FAILURE_PROCESS_ID_REGEX.replace_all(&without_ts, "${1}${2}<id>");
    let without_req_id = FAILURE_REQUEST_ID_REGEX.replace_all(&without_pid, "${1}${2}<id>");
    let without_duration_kv =
        FAILURE_DURATION_KV_REGEX.replace_all(&without_req_id, "${1}${2}<duration>");
    let without_subsec_brackets = FAILURE_BRACKETED_SUBSEC_DURATION_REGEX
        .replace_all(&without_duration_kv, "${1}<duration>${2}");
    FAILURE_ATTEMPT_COUNTER_REGEX
        .replace_all(&without_subsec_brackets, "${1}${2}<n>")
        .into_owned()
}

fn signature(
    name: &str,
    arguments: &serde_json::Value,
    outcome: &ToolExecutionOutcome,
) -> Signature {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    outcome.success.hash(&mut hasher);
    // Hash the full scrubbed result, before display truncation. The fork keeps
    // raw outcomes for tool consumers; only hashes survive this boundary.
    // Do not strip arbitrary fields that might distinguish real progress.
    let scrubbed = crate::agent::turn::scrub_credentials(&outcome.output);
    if outcome.success {
        scrubbed.hash(&mut hasher);
    } else {
        normalize_failure_observation_for_signature(&scrubbed).hash(&mut hasher);
    }
    Signature {
        call: call_signature(name, arguments),
        result: hasher.finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(output: &str) -> ToolExecutionOutcome {
        ToolExecutionOutcome {
            output: output.into(),
            output_data: None,
            success: false,
            error_reason: Some(output.into()),
            duration: std::time::Duration::ZERO,
            receipt: None,
        }
    }

    #[test]
    fn signature_canonicalizes_arguments_without_stripping_tool_fields() {
        assert_eq!(
            call_signature(
                "shell",
                &serde_json::json!({"command":"echo x", "approved":true})
            ),
            call_signature(
                "shell",
                &serde_json::json!({"approved":false, "command":"echo x"})
            ),
        );
        assert_ne!(
            call_signature("remote", &serde_json::json!({"approved":true})),
            call_signature("remote", &serde_json::json!({"approved":false})),
            "approved is only removed for runtime-owned approval arguments"
        );
        assert_eq!(
            call_signature(
                "remote",
                &serde_json::from_str(r#"{"b":2,"a":{"y":2,"x":1}}"#).unwrap()
            ),
            call_signature(
                "remote",
                &serde_json::from_str(r#"{"a":{"x":1,"y":2},"b":2}"#).unwrap()
            ),
        );
        assert_ne!(
            call_signature("remote", &serde_json::json!({"timestamp":1})),
            call_signature("remote", &serde_json::json!({"timestamp":2})),
            "tool-owned fields must remain meaningful"
        );
    }

    #[test]
    fn evidence_hashes_full_scrubbed_output_before_display_truncation() {
        let args = serde_json::json!({});
        let first = outcome("api_key = \"sk-test-12345678901234567890\"; failure");
        let second = outcome("api_key = \"sk-test-09876543210987654321\"; failure");
        assert!(signature("remote", &args, &first) == signature("remote", &args, &second));
        assert!(
            first.output.contains("12345678901234567890"),
            "raw outcome is not changed"
        );

        let prefix = "x".repeat(40_000);
        let first = outcome(&format!("{prefix}: error A"));
        let second = outcome(&format!("{prefix}: error B"));
        assert!(signature("remote", &args, &first) != signature("remote", &args, &second));
    }

    #[test]
    fn synthetic_outcomes_neither_warn_nor_spend_pending_recovery() {
        let call = ParsedToolCall {
            name: "remote".into(),
            arguments: serde_json::json!({}),
            tool_call_id: Some("call".into()),
        };
        let mut guard = RepetitionGuard::default();
        let observe = |guard: &mut RepetitionGuard, executed: bool| {
            guard.observe_round(
                std::slice::from_ref(&call),
                &[CompletedToolExecution {
                    outcome: outcome("same error"),
                    executed_tool_name: executed.then(|| "remote".into()),
                }],
                &HashSet::new(),
                1,
            )
        };
        assert!(observe(&mut guard, false).message_key.is_none());
        assert_eq!(
            observe(&mut guard, true).message_key,
            Some("turn-repeated-failure-recovery")
        );
        assert!(observe(&mut guard, false).message_key.is_none());
        assert!(!guard.is_exhausted());
        assert!(observe(&mut guard, true).close);
    }

    #[test]
    fn failure_signature_normalizes_volatile_telemetry_without_collapsing_distinct_errors() {
        let args = serde_json::json!({"command": "curl https://api.example.test/v1/items"});
        let attempt_one = outcome(
            "[2026-11-08T03:50:07.120Z] pid=41201 tid:99 request_id=req-88a1 (attempt 1/3) \
             connect ECONNREFUSED 10.0.0.8:443 failed in 142ms [142.4ms]",
        );
        let attempt_two = outcome(
            "[2026-11-08 03:50:09.904 UTC] pid=41289 tid:104 request_id=req-99b2 (attempt 2/3) \
             connect ECONNREFUSED 10.0.0.8:443 failed in 1.24s [98.1ms]",
        );
        assert!(
            signature("shell", &args, &attempt_one) == signature("shell", &args, &attempt_two),
            "volatile wrapper telemetry on identical failures must converge to one signature"
        );

        let different_exit_code =
            outcome("[2026-11-08T03:50:07Z] pid=41201 command failed with exit status 1");
        let different_exit_code_two =
            outcome("[2026-11-08T03:50:08Z] pid=41202 command failed with exit status 2");
        assert!(
            signature("shell", &args, &different_exit_code)
                != signature("shell", &args, &different_exit_code_two),
            "distinct exit codes must remain distinct failure evidence"
        );

        let different_timeout_window =
            outcome("request timed out in 30s after 30s (elapsed: 30001ms)");
        let different_timeout_window_two =
            outcome("request timed out in 60s after 60s (elapsed: 60004ms)");
        assert!(
            signature("shell", &args, &different_timeout_window)
                != signature("shell", &args, &different_timeout_window_two),
            "configured timeout windows must not be stripped as elapsed telemetry"
        );

        let success_one = ToolExecutionOutcome {
            output: "2026-11-08T03:50:07Z pid=41201 status=running elapsed=10ms".into(),
            output_data: None,
            success: true,
            error_reason: None,
            duration: std::time::Duration::ZERO,
            receipt: None,
        };
        let success_two = ToolExecutionOutcome {
            output: "2026-11-08T03:50:08Z pid=41202 status=running elapsed=20ms".into(),
            output_data: None,
            success: true,
            error_reason: None,
            duration: std::time::Duration::ZERO,
            receipt: None,
        };
        assert!(
            signature("shell", &args, &success_one) != signature("shell", &args, &success_two),
            "successful outputs must never have timestamps, PIDs, or durations normalized"
        );
    }
}
