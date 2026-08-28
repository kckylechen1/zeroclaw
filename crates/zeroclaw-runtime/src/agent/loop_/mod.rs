/// Format token count with thousands separators.
pub(crate) fn format_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
use std::sync::{Arc, LazyLock, Mutex};
// Test suites under `loop_/tests.rs` pull these through `use super::*`.
#[cfg(test)]
pub(crate) use crate::agent::TurnMeta;
#[cfg(test)]
pub(crate) use crate::approval::ApprovalManager;
#[cfg(test)]
pub(crate) use crate::observability::{self as observability, Observer, ObserverEvent};
#[cfg(test)]
pub(crate) use crate::tools;
#[cfg(test)]
pub(crate) use crate::tools::Tool;
#[cfg(test)]
pub(crate) use std::collections::HashSet;
#[cfg(test)]
pub(crate) use tokio_util::sync::CancellationToken;
#[cfg(test)]
pub(crate) use zeroclaw_api::ingress::{IngressContext, TurnOrigin};
#[cfg(test)]
pub(crate) use zeroclaw_config::schema::Config;
#[cfg(test)]
pub(crate) use zeroclaw_providers::{ChatRequest, ModelProvider};

mod agent_turn;
mod process_message;
mod run;
mod run_overrides;

// Cost tracking moved to `super::cost`.
pub use super::cost::{
    TOOL_LOOP_COST_TRACKING_CONTEXT, ToolLoopCostTrackingContext, TurnUsage,
    check_tool_loop_budget, record_tool_loop_cost_usage,
};

// History management moved to `super::history`.
pub use super::history::{
    append_or_merge_system_message, canonicalize_tool_result_media_markers,
    estimate_history_tokens, load_interactive_session_history, normalize_system_messages,
    save_interactive_session_history, trim_history, truncate_tool_result,
};

// Tool / MCP filter admission moved to `super::tool_filter`.
#[cfg(test)]
pub(crate) use super::tool_filter::glob_match;
pub use super::tool_filter::{
    append_pinned_mcp_section, apply_policy_tool_filter, eager_mcp_tool_allowed,
    filter_by_allowed_tools, filter_tool_specs_for_turn, mcp_tool_access_policy,
    register_eager_mcp_tool_if_allowed,
};
pub(crate) use super::tool_filter::{
    compute_excluded_mcp_tools, mcp_allowed_tool_count, preactivate_always_filter_groups,
};

// Text-protocol tool prompt helpers moved to `super::text_tool_prompt`.
pub(crate) use super::text_tool_prompt::retain_registered_tool_descriptions;

// Bounded interactive line IO moved to `super::capped_line`.
pub(crate) use super::capped_line::{CappedLine, MAX_INTERACTIVE_INPUT_BYTES, read_capped_line};

// Channel / peripheral factories moved to `super::channel_factories`.
pub use super::channel_factories::{
    CLI_CHANNEL_FN, PeripheralToolsFn, load_peripheral_tools, register_channel_map_fn,
    register_cli_channel_fn, register_peripheral_tools_fn,
};
pub(crate) use super::channel_factories::{live_channel_registry, seed_channel_handles};

// Prompt / export helpers moved to `super::prompt_helpers`.
#[cfg(test)]
pub(crate) use super::prompt_helpers::tools_to_openai_format;
pub(crate) use super::prompt_helpers::{
    autosave_memory_key, build_hardware_context, build_system_prompt_for_turn, capture_llm_messages,
};
pub use super::prompt_helpers::{make_query_summary, native_tool_specs_present_for_turn};

pub use super::text_tool_prompt::{
    apply_text_tool_prompt_policy, build_tool_instructions, build_tool_instructions_for_names,
};

/// Minimum user-message length (in chars) for auto-save to memory.
/// Matches the channel-side constant in `channels/mod.rs`.
pub(crate) const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;

// Session scope + announcement claims moved to `super::announce_claim`.
#[cfg(test)]
pub(crate) use super::announce_claim::CHILD_ANNOUNCEMENT_STORE_TEST_HOOK;
pub use super::announce_claim::{
    TOOL_LOOP_SESSION_KEY, TOOL_LOOP_THREAD_ID, TurnOutcome, UnclaimOnDrop,
    claim_announcements_for_scoped_turn, scope_session_key, scope_thread_id,
    settle_announcement_guards,
};
pub(crate) use super::announce_claim::{
    claim_announcements_for_turn, current_session_key, session_key_is_scoped,
    synthetic_session_key_for_run,
};

// Re-export tool call parsing from the standalone parser crate.
pub use zeroclaw_tool_call_parser::{
    ParsedToolCall, ToolProtocolEnvelopeKind, build_native_assistant_history_from_parsed_calls,
    canonicalize_json_for_tool_signature, classify_tool_protocol_envelope,
    contains_tool_protocol_tag_call, detect_tool_call_parse_issue,
    looks_like_malformed_tool_protocol_envelope,
    looks_like_malformed_tool_protocol_envelope_for_known_tools, looks_like_tool_protocol_envelope,
    looks_like_tool_protocol_example, parse_tool_calls, strip_think_tags, strip_tool_result_blocks,
    tool_protocol_envelope_mentions_known_tool,
};

/// Test seam: the fully enriched user message a turn is about to send, so the
/// `run`/`process_message` entry points can be asserted on without a live
/// model provider (the `Agent` pipeline is covered by capturing providers).
#[cfg(test)]
type TurnUserMessageTestHook = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
pub(crate) static TURN_USER_MESSAGE_TEST_HOOK: LazyLock<Mutex<Option<TurnUserMessageTestHook>>> =
    LazyLock::new(|| Mutex::new(None));

/// Shared fixture for the waker's tests, here rather than in a `mod tests` so
/// both entry-point suites (`loop_`'s and `agent`'s) install the announce hooks
/// the same way and take the same lock.
#[cfg(test)]
pub(crate) mod announce_test_support {
    use super::{CHILD_ANNOUNCEMENT_STORE_TEST_HOOK, TURN_USER_MESSAGE_TEST_HOOK};
    use crate::control_plane::{SqliteTaskStore, TaskKind, TaskRecord, TaskRegistry, TaskStatus};
    use std::sync::{Arc, Mutex, MutexGuard};

    /// The hooks are process-global, so the tests that install them run one at
    /// a time.
    static SERIALIZE: Mutex<()> = Mutex::new(());

    pub(crate) struct AnnounceFixture {
        _guard: MutexGuard<'static, ()>,
        pub(crate) store: Arc<SqliteTaskStore>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl AnnounceFixture {
        /// Installs a real in-memory control-plane store and a capture hook for
        /// whatever user message the next turn builds.
        ///
        /// The store goes in through the test seam rather than
        /// `init_control_plane`, which is a `OnceLock`: installing it here
        /// would leak into every other test in this binary and break
        /// `control_plane::global`'s `uninitialized_is_none`.
        pub(crate) fn install() -> Self {
            let guard = SERIALIZE.lock().unwrap_or_else(|e| e.into_inner());
            let store = Arc::new(SqliteTaskStore::new_in_memory().expect("in-memory store"));
            *CHILD_ANNOUNCEMENT_STORE_TEST_HOOK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(store.clone() as Arc<dyn TaskRegistry>);
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let sink = Arc::clone(&seen);
            *TURN_USER_MESSAGE_TEST_HOOK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(move |msg: &str| {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(msg.to_string());
            }));
            Self {
                _guard: guard,
                store,
                seen,
            }
        }

        /// As [`Self::install`], but with no store at all: the "no daemon" shape.
        pub(crate) fn install_without_control_plane() -> Self {
            let fixture = Self::install();
            *CHILD_ANNOUNCEMENT_STORE_TEST_HOOK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            fixture
        }

        /// A finished, undelivered child filed under `parent` — exactly the row
        /// shape `claim_undelivered_children` selects on.
        pub(crate) async fn finished_child(&self, id: &str, parent: &str, output: &str) {
            let store: &dyn TaskRegistry = self.store.as_ref();
            store
                .create(TaskRecord {
                    id: id.to_string(),
                    kind: TaskKind::Delegate,
                    agent: "worker".to_string(),
                    status: TaskStatus::Running,
                    owner_pid: std::process::id(),
                    owner_boot_id: "boot-test".to_string(),
                    heartbeat_at: None,
                    depth: 1,
                    parent_id: Some(parent.to_string()),
                    originator_route: None,
                    delivered: false,
                    idem_key: None,
                    principal_id: None,
                    executor: None,
                    started_at: chrono::Utc::now().to_rfc3339(),
                    finished_at: None,
                })
                .await
                .expect("create child");
            store
                .update_status(id, TaskStatus::Completed, Some(output.to_string()), None)
                .await
                .expect("finish child");
        }

        /// What is still claimable under `parent`, claiming it in the process.
        pub(crate) async fn claim(
            &self,
            parent: &str,
        ) -> Vec<zeroclaw_api::announce::Announcement> {
            let store: &dyn TaskRegistry = self.store.as_ref();
            store
                .claim_undelivered_children(parent)
                .await
                .expect("claim")
        }

        /// The store as the guard takes it, for tests that drive
        /// [`super::UnclaimOnDrop`] directly.
        pub(crate) fn store_handle(&self) -> Arc<dyn TaskRegistry> {
            Arc::clone(&self.store) as Arc<dyn TaskRegistry>
        }

        /// Whether `id` currently reads as delivered. Read-only — unlike
        /// [`Self::claim`] it does not consume the announcement.
        pub(crate) async fn is_delivered(&self, id: &str) -> bool {
            let store: &dyn TaskRegistry = self.store.as_ref();
            store
                .get(id)
                .await
                .expect("get task")
                .expect("task exists")
                .delivered
        }

        /// Wait for `id` to be returned to the store by a dropped guard.
        ///
        /// The unclaim rides a detached task (a destructor cannot await), so a
        /// test has to give the runtime a chance to run it. Bounded: returns
        /// `false` rather than hanging if it never lands, so a broken guard
        /// fails the assertion instead of the test timing out.
        pub(crate) async fn wait_until_returned(&self, id: &str) -> bool {
            for _ in 0..200 {
                if !self.is_delivered(id).await {
                    return true;
                }
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            false
        }

        /// User messages captured so far that carry `marker`. The filter keeps
        /// an unrelated concurrently-running turn out of the result.
        pub(crate) fn messages_containing(&self, marker: &str) -> Vec<String> {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|msg| msg.contains(marker))
                .cloned()
                .collect()
        }
    }

    impl Drop for AnnounceFixture {
        fn drop(&mut self) {
            *CHILD_ANNOUNCEMENT_STORE_TEST_HOOK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            *TURN_USER_MESSAGE_TEST_HOOK
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}

/// Report the enriched user message to the test hook, when one is installed.
#[allow(unused_variables)]
pub(crate) fn observe_turn_user_message(enriched: &str) {
    #[cfg(test)]
    {
        let hook = TURN_USER_MESSAGE_TEST_HOOK
            .lock()
            .expect("turn user-message test hook lock should not be poisoned")
            .clone();
        if let Some(hook) = hook {
            hook(enriched);
        }
    }
}

pub use zeroclaw_api::TOOL_CHOICE_OVERRIDE;

// Tool execution moved to `super::tool_execution`.
pub use super::tool_execution::{ToolExecutionOutcome, should_execute_tools_in_parallel};

// agent_turn entry moved to `agent_turn`.
pub use self::agent_turn::agent_turn;

// Run overrides / resolve helpers moved to `run_overrides`.
pub use self::process_message::process_message;
pub use self::run::run;
pub use self::run_overrides::AgentRunOverrides;
#[cfg(test)]
pub(crate) use self::run_overrides::RESOLVED_AGENT_FOR_TURN_TEST_HOOK;
pub(crate) use self::run_overrides::{
    agent_provider_composite, api_key_and_uri_for_provider, resolved_agent_for_turn,
};

// ── Agent Tool-Call Loop ──────────────────────────────────────────────────
// The turn engine lives in `super::turn` — `run_tool_call_loop` plus one
// file per step (run sheet in agent/turn/mod.rs). `crate::agent::loop_`
// stays the canonical public path via these re-exports.
pub(crate) use super::turn::StreamCancelledAfterOutput;
#[cfg(test)]
pub(crate) use super::turn::{
    DEFAULT_MAX_TOOL_ITERATIONS, MAX_MALFORMED_TOOL_PROTOCOL_RETRIES,
    build_native_assistant_history, consume_provider_streaming_response,
    maybe_inject_channel_delivery_defaults, resolve_display_text,
};
pub use super::turn::{
    DraftEvent, LoopKnobs, MaxIterationBehavior, ModelSwitchCallback, ModelSwitchRequested,
    PROGRESS_MIN_INTERVAL_MS, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess,
    ResolvedRuntimeKnobs, StreamDelta, ToolLoop, ToolLoopCancelled, drain_steering_messages,
    is_model_switch_requested, is_tool_loop_cancelled, run_tool_call_loop, scrub_credentials,
};

// Heavy suite gated so lib-test iteration does not pay 13.9k lines; CI runtime leg enables it.
#[cfg(all(test, feature = "heavy-tests"))]
mod tests;
