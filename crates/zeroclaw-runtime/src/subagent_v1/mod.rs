//! V1 bounded ReasoningSubAgent vertical (frozen contract, rev 3).
//!
//! One bounded local reasoning run, end to end:
//!
//! ```text
//! Parent
//!   → SubAgentProfileV1     admitted, immutable                     (SA-3/SA-4)
//!   → ContextBundleV1       digest-bound content snapshot           (SA-18/SA-19)
//!   → admit()               the FIRST privacy boundary: Private     (SA-14/SA-15)
//!                           Dyad / AgentSoul refs rejected, the
//!                           AdmittedContextBundleV1 cannot express them
//!   → lineage_ref           the ONE depth authority                 (SA-9)
//!   → ReasoningSubAgent     run-scoped principal                    (SA-13)
//!   → SubAgentReportV1      the ONLY child→parent result channel    (SA-21)
//!   → Parent                disposition of findings/candidates      (SA-22)
//! ```
//!
//! Authority boundaries encoded here:
//! - The profile is the only capability source (SA-5) and the V1 child
//!   tool catalog is EMPTY — V1 reasoning runs execute no tools (recorded
//!   least-authority reading of the contract; the materialized set is
//!   set-equal to the profile's declared list by construction).
//! - The execution context accepts EXACTLY the six SA-6 inputs. No
//!   `Config`, tool registry, channel map, or memory backend crosses the
//!   parent→child boundary (compile-level signature test below).
//! - Model access is a host-resolved opaque binding (SA-7d): the child
//!   never holds provider configuration or credential material.
//! - Nothing durable: this path writes no Task/Attempt rows anywhere
//!   (SA-26). The module does not touch the control plane.
//! - Children of V1 cannot spawn children (SA-12/D1): run admission
//!   refuses any spawning lineage deeper than the parent (depth 0).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zeroclaw_api::subagent_v1::{
    AdmittedContextBundleV1, BundleRedactionPolicy, ContextBundleV1, ContextClassV1, Finding,
    LineageRef, ModelPolicyV1, ParentRunRef, ProposedCandidate, Recommendation,
    ReportChannelMessage, SubAgentBudgetV1, SubAgentProfileV1, SubAgentReportV1, SubAgentRoleV1,
    SubAgentRunRef, SubAgentTerminalFact, SubAgentToolNameV1, SubAgentToolPolicyV1, SubAgentUsage,
    VersionedProfileRef,
};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::Config;

// ─────────────────────────────────────────────────────────────────────────
// Lineage helpers (SA-9): ambient scope for shared-registry contexts
// ─────────────────────────────────────────────────────────────────────────

// Ambient spawn lineage for contexts that execute with SHARED parent
// tool instances — the legacy bounded-delegate handout, where children
// receive the parent's tool Arcs without a registry rebuild (census
// §2.1). The bounded sub-loop scopes this around its tool-call loop so
// spawn-capable inherited tools observe the CHILD's depth, not the
// parent's. Registries built with an explicit lineage (every
// `agent::run` rebuild) carry their own; the ambient value never
// LOWERS an explicitly carried lineage (readers take the max).
tokio::task_local! {
    pub(crate) static AMBIENT_SPAWN_LINEAGE: LineageRef;
}

/// The ambient lineage visible to shared tool instances, if any.
pub(crate) fn ambient_lineage() -> Option<LineageRef> {
    AMBIENT_SPAWN_LINEAGE
        .try_with(|lineage| lineage.clone())
        .ok()
}

/// The effective depth for a spawn-capable tool instance: never lower
/// than its own carried lineage or the ambient scope (SA-9 — one
/// monotonic ledger; sharing an Arc into a deeper context must not
/// make the context behave shallower).
pub(crate) fn effective_depth_with_ambient(own: Option<&LineageRef>) -> u32 {
    let own_depth = own.map_or(0, LineageRef::depth);
    let ambient_depth = ambient_lineage().as_ref().map_or(0, LineageRef::depth);
    own_depth.max(ambient_depth)
}

// ─────────────────────────────────────────────────────────────────────────
// Opaque model access binding (SA-7d)
// ─────────────────────────────────────────────────────────────────────────

/// A bounded completion request. Carries only prompt content and sampling
/// hints — no credentials, no provider configuration.
#[derive(Debug, Clone)]
pub struct BoundedModelRequest {
    pub system: String,
    pub user: String,
    pub temperature: Option<f64>,
}

/// A bounded completion response with the usage the meter needs.
#[derive(Debug, Clone)]
pub struct BoundedModelResponse {
    pub text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Host-side model resolution (SA-7d): resolves the profile's
/// `model_policy` reference AT USE TIME. Implemented by the trusted host
/// against the operator's provider configuration; the child context holds
/// only the opaque binding, never this resolver.
#[async_trait]
pub trait ModelAccessResolver: Send + Sync {
    async fn complete(&self, request: BoundedModelRequest) -> anyhow::Result<BoundedModelResponse>;

    /// Non-secret identity of the resolved reference (the `family.alias`
    /// string). Never a credential.
    fn provider_ref(&self) -> &str;
}

/// The opaque model-access binding handed to the run driver. Its public
/// surface is exactly: resolve one bounded completion, and echo the
/// non-secret provider reference. No credential material is reachable.
pub struct OpaqueModelBinding {
    resolver: Arc<dyn ModelAccessResolver>,
}

impl std::fmt::Debug for OpaqueModelBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted by construction: only the non-secret reference prints.
        f.debug_struct("OpaqueModelBinding")
            .field("provider_ref", &self.provider_ref())
            .finish_non_exhaustive()
    }
}

impl OpaqueModelBinding {
    #[must_use]
    pub fn new(resolver: Arc<dyn ModelAccessResolver>) -> Self {
        Self { resolver }
    }

    #[must_use]
    pub fn provider_ref(&self) -> &str {
        self.resolver.provider_ref()
    }

    async fn complete(&self, request: BoundedModelRequest) -> anyhow::Result<BoundedModelResponse> {
        self.resolver.complete(request).await
    }
}

/// Host resolver over the operator's provider configuration. Holds the
/// config privately (host side); the child context never sees this type.
pub struct ConfigModelAccessResolver {
    config: Arc<Config>,
    policy: ModelPolicyV1,
}

impl ConfigModelAccessResolver {
    #[must_use]
    pub fn new(config: Arc<Config>, policy: ModelPolicyV1) -> Self {
        Self { config, policy }
    }
}

#[async_trait]
impl ModelAccessResolver for ConfigModelAccessResolver {
    async fn complete(&self, request: BoundedModelRequest) -> anyhow::Result<BoundedModelResponse> {
        use zeroclaw_api::model_provider::{ChatMessage, ChatRequest};

        let (family, alias) = self.policy.provider_ref.split_once('.').ok_or_else(|| {
            anyhow::Error::msg(format!(
                "model_policy.provider_ref {:?} is not family.alias",
                self.policy.provider_ref
            ))
        })?;
        let entry = self
            .config
            .providers
            .models
            .iter_entries()
            .find(|(ty, al, _)| *ty == family && *al == alias)
            .map(|(_, _, entry)| entry)
            .ok_or_else(|| {
                anyhow::Error::msg(format!(
                    "model_policy.provider_ref {:?} names no configured provider",
                    self.policy.provider_ref
                ))
            })?;
        let credential = entry.api_key.clone();
        let options =
            zeroclaw_providers::provider_runtime_options_for_alias(&self.config, family, alias);
        let provider = zeroclaw_providers::create_model_provider_for_alias(
            &self.config,
            family,
            alias,
            credential.as_deref(),
            &options,
        )?;
        // Resolve the configured model for the referenced alias when
        // the profile does not pin one — and FAIL CLOSED when neither
        // supplies a non-empty model: dispatching an empty model
        // identifier is an invalid request, never a guess.
        let model = match self.policy.model.clone() {
            Some(model) if !model.trim().is_empty() => model,
            _ => self
                .config
                .providers
                .models
                .iter_entries()
                .find(|(ty, al, _)| *ty == family && *al == alias)
                .and_then(|(_, _, entry)| entry.model.clone())
                .unwrap_or_default(),
        };
        if model.trim().is_empty() {
            return Err(anyhow::Error::msg(format!(
                "no model configured for provider_ref {:?}: neither the profile's \
                 model_policy nor the provider alias pins a model; refusing to \
                 dispatch an empty model identifier",
                self.policy.provider_ref
            )));
        }
        let messages = vec![
            ChatMessage::system(request.system),
            ChatMessage::user(request.user),
        ];
        let chat_request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        // Provider-dispatch contract: model calls go through
        // `ProviderDispatch` so attribution spans attach (never direct
        // `ModelProvider` method calls).
        let dispatcher = zeroclaw_providers::ProviderDispatch::from_ref(provider.as_ref());
        let response = dispatcher
            .chat(chat_request, &model, request.temperature)
            .await?;
        let (tokens_in, tokens_out) = response
            .usage
            .map(|usage| {
                (
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        if tokens_in == 0 && tokens_out == 0 {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "subagent_v1: model binding resolved no token usage; token budget is enforced over counted usage only"
            );
        }
        Ok(BoundedModelResponse {
            text: response.text.unwrap_or_default(),
            tokens_in,
            tokens_out,
        })
    }

    fn provider_ref(&self) -> &str {
        &self.policy.provider_ref
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Budget meter (SA-8/SA-27)
// ─────────────────────────────────────────────────────────────────────────

/// Shared read-accounting meter (SA-8), scoped to ONE run: the SAME
/// `Arc` is held by the parent-side spawn admission and the child run,
/// so child consumption counts against that run's budget and
/// exhaustion terminates the run. The meter is minted fresh per
/// `SubAgentRunRef` and discarded at the run's terminal — it is
/// single-run state, NEVER process-lifetime quota: a run's 120s time
/// ceiling must not moonlight as an aggregate. Aggregate quotas
/// (per-ParentRun spawn counts, hourly token caps) are a separate
/// future `ParentRunQuota`/`RateBudget` concept, out of scope here.
pub struct SubAgentBudgetMeter {
    budget: SubAgentBudgetV1,
    state: Mutex<MeterState>,
}

#[derive(Clone, Copy)]
struct MeterState {
    actions_used: u32,
    tokens_in: u64,
    tokens_out: u64,
    started_at: std::time::Instant,
}

impl SubAgentBudgetMeter {
    #[must_use]
    pub fn new(budget: SubAgentBudgetV1) -> Self {
        Self::new_with_start(budget, std::time::Instant::now())
    }

    /// Constructor with an injected start instant: hosts and tests
    /// advance the meter's clock deterministically instead of sleeping.
    /// The meter is SINGLE-RUN state (see [`ReasoningSubagentTool`]):
    /// whatever start it gets, it is discarded at the run's terminal —
    /// no cross-run reuse exists to carry a backdated clock into
    /// another run.
    #[must_use]
    pub fn new_with_start(budget: SubAgentBudgetV1, started_at: std::time::Instant) -> Self {
        Self {
            budget,
            state: Mutex::new(MeterState {
                actions_used: 0,
                tokens_in: 0,
                tokens_out: 0,
                started_at,
            }),
        }
    }

    /// The budget this meter enforces (all three ceilings — SA-27).
    #[must_use]
    pub fn budget(&self) -> SubAgentBudgetV1 {
        self.budget
    }

    /// Record one billable action. `false` = the action ceiling is
    /// exhausted; the caller must not run the action.
    pub fn try_record_action(&self) -> bool {
        let mut state = self.state.lock();
        if state.actions_used >= self.budget.max_actions {
            return false;
        }
        state.actions_used += 1;
        true
    }

    /// Record token usage. `false` = the token ceiling is (now) exceeded;
    /// budget exhaustion terminates the run `timed_out` (SA-23).
    pub fn record_tokens(&self, tokens_in: u64, tokens_out: u64) -> bool {
        let mut state = self.state.lock();
        state.tokens_in = state.tokens_in.saturating_add(tokens_in);
        state.tokens_out = state.tokens_out.saturating_add(tokens_out);
        state.tokens_in.saturating_add(state.tokens_out) <= self.budget.max_tokens
    }

    /// Wall-clock exhaustion (SA-23: `timed_out` is budget exhaustion).
    #[must_use]
    pub fn time_exhausted(&self) -> bool {
        let state = self.state.lock();
        state.started_at.elapsed().as_secs() >= self.budget.time_limit_secs
    }

    #[must_use]
    pub fn exhausted(&self) -> bool {
        let state = self.state.lock();
        state.actions_used >= self.budget.max_actions
            || state.tokens_in.saturating_add(state.tokens_out) >= self.budget.max_tokens
            || state.started_at.elapsed().as_secs() >= self.budget.time_limit_secs
    }

    #[must_use]
    pub fn usage(&self) -> SubAgentUsage {
        let state = self.state.lock();
        SubAgentUsage {
            elapsed_ms: state.started_at.elapsed().as_millis() as u64,
            tokens_in: state.tokens_in,
            tokens_out: state.tokens_out,
            actions: state.actions_used,
        }
    }

    /// Remaining wall-clock, for the run's unit timeout.
    fn remaining_time(&self) -> std::time::Duration {
        let state = self.state.lock();
        let elapsed = state.started_at.elapsed();
        let ceiling = std::time::Duration::from_secs(self.budget.time_limit_secs);
        ceiling.checked_sub(elapsed).unwrap_or_default()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Control states (SA-23)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct ControlInner {
    graceful_stop_requested: bool,
    abort_requested: bool,
}

/// Parent-held control handle: raise the two control events (SA-23).
#[derive(Clone)]
pub struct SubAgentControlHandle {
    inner: Arc<Mutex<ControlInner>>,
}

impl SubAgentControlHandle {
    pub fn request_graceful_stop(&self) {
        self.inner.lock().graceful_stop_requested = true;
    }

    pub fn request_abort(&self) {
        self.inner.lock().abort_requested = true;
    }
}

/// Validate a terminal-fact write (SA-23): `stopped`/`aborted` require
/// their matching control event, `timed_out` requires budget exhaustion;
/// `completed`/`failed` need no control event. No path may write a
/// terminal fact around this validator.
pub fn validate_terminal_transition(
    fact: SubAgentTerminalFact,
    graceful_stop_seen: bool,
    abort_seen: bool,
    budget_exhausted: bool,
) -> Result<(), InvalidTerminalTransition> {
    match fact {
        SubAgentTerminalFact::Stopped if !graceful_stop_seen => Err(InvalidTerminalTransition {
            fact,
            reason: "stopped written without a graceful_stop_requested control event",
        }),
        SubAgentTerminalFact::Aborted if !abort_seen => Err(InvalidTerminalTransition {
            fact,
            reason: "aborted written without an abort_requested control event",
        }),
        SubAgentTerminalFact::TimedOut if !budget_exhausted => Err(InvalidTerminalTransition {
            fact,
            reason: "timed_out written without budget exhaustion",
        }),
        _ => Ok(()),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid terminal transition for {fact:?}: {reason}")]
pub struct InvalidTerminalTransition {
    pub fact: SubAgentTerminalFact,
    pub reason: &'static str,
}

// ─────────────────────────────────────────────────────────────────────────
// Child tool set (SA-5/SA-7a/SA-7b)
// ─────────────────────────────────────────────────────────────────────────

/// The V1 child tool catalog. EMPTY: V1 reasoning runs execute no tools
/// (least-authority reading — the run is a bounded reasoning unit over
/// the bundle; any tool execution waits for a profile class that admits
/// one). Admission refuses a declared list this catalog cannot satisfy,
/// so the materialized set is set-equal to the declared list by
/// construction, with nothing extra (SA-5).
pub const V1_CHILD_TOOL_CATALOG: &[&str] = &[];

/// The child's materialized tool set. Built from the profile ALONE
/// (SA-7a): the constructor's only input is the profile ref — there is
/// no parameter through which a parent tool Arc could enter, so no
/// child tool's Arc identity can be the parent's instance.
#[derive(Debug, Clone, Default)]
pub struct ChildToolSet {
    names: Vec<SubAgentToolNameV1>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChildToolSetError {
    #[error(
        "v1 child tool catalog is empty in this vertical: declared tool {name:?} cannot be \
         materialized; V1 reasoning runs execute no tools"
    )]
    NotInV1Catalog { name: String },
}

impl ChildToolSet {
    /// The child-registry builder. Takes ONLY a profile ref (SA-7a).
    /// Every declared name must be materializable from the v1 catalog;
    /// the v1 catalog is empty, so every admitted v1 profile declares an
    /// empty list and the materialized set is empty — set equality with
    /// nothing extra (SA-5), and `spawn_subagent`/`delegate` can never
    /// appear because the tool-name type refuses them at parse time
    /// (SA-7b/SA-12).
    pub fn from_profile(profile: &SubAgentProfileV1) -> Result<Self, ChildToolSetError> {
        let mut names = Vec::new();
        for declared in &profile.tool_policy.tools {
            if !V1_CHILD_TOOL_CATALOG
                .iter()
                .any(|catalog| *catalog == declared.as_str())
            {
                return Err(ChildToolSetError::NotInV1Catalog {
                    name: declared.as_str().to_string(),
                });
            }
            if !names.iter().any(|n| n == declared) {
                names.push(declared.clone());
            }
        }
        Ok(Self { names })
    }

    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.names.iter().map(SubAgentToolNameV1::as_str).collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Hooks (SA-28): deny/narrow/redact/log only
// ─────────────────────────────────────────────────────────────────────────

/// A hook decision. The enum has NO variant that can widen a profile or
/// grant a capability — widening is structurally unrepresentable, and
/// the application code intersects, never unions.
#[derive(Debug, Clone)]
pub enum SubAgentHookDecision {
    Allow,
    Deny {
        reason_code: String,
    },
    /// Drop context classes from the bundle projection (narrow only).
    NarrowContext {
        drop_classes: Vec<ContextClassV1>,
    },
    /// Apply additional redaction to the bundle projection.
    RedactBundle {
        redaction: BundleRedactionPolicy,
    },
    /// Log a note; no effect on admission.
    Log {
        note_code: String,
    },
}

#[async_trait]
pub trait SubAgentV1Hook: Send + Sync {
    /// Called at admission with the profile and the proposed bundle.
    async fn on_admission(
        &self,
        profile: &SubAgentProfileV1,
        bundle: &ContextBundleV1,
    ) -> SubAgentHookDecision;
}

/// Apply a hook decision to a bundle. Can only deny, narrow (add
/// exclusions), or redact — the resulting bundle is recomputed and
/// re-digested. A decision that would widen anything has no
/// representation, so a widening hook result is discarded by
/// construction (SA-28).
pub fn apply_hook_decision(
    decision: SubAgentHookDecision,
    bundle: ContextBundleV1,
) -> anyhow::Result<ContextBundleV1> {
    match decision {
        SubAgentHookDecision::Allow | SubAgentHookDecision::Log { .. } => Ok(bundle),
        SubAgentHookDecision::Deny { reason_code } => Err(anyhow::Error::msg(format!(
            "hook denied admission: {reason_code}"
        ))),
        SubAgentHookDecision::NarrowContext { drop_classes } => {
            let mut narrowed = bundle;
            for class in drop_classes {
                if !narrowed.explicit_exclusions.contains(&class) {
                    narrowed.explicit_exclusions.push(class);
                }
            }
            narrowed.digest = narrowed.compute_digest();
            Ok(narrowed)
        }
        SubAgentHookDecision::RedactBundle { redaction } => {
            let mut redacted = bundle;
            redacted.redaction_policy = redaction;
            redacted.digest = redacted.compute_digest();
            Ok(redacted)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Profile registry and admission (SA-3/SA-4/SA-5)
// ─────────────────────────────────────────────────────────────────────────

/// The admitted profile registry (SA-3). Admission validates the frozen
/// profile law; runs are constructible only from an admitted
/// [`VersionedProfileRef`] resolved here.
#[derive(Default)]
pub struct SubAgentProfileRegistry {
    revisions: HashMap<String, Vec<SubAgentProfileV1>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileAdmissionError {
    #[error("profile digest does not verify: {0}")]
    Digest(String),
    #[error("reasoning profile {profile_id:?} carries a supervisor authority set (SA-29)")]
    SupervisorAuthorityOnReasoning { profile_id: String },
    #[error(
        "supervisor profile {profile_id:?} carries an empty authority set (SA-29: a \
         Supervisor's Tachi authority is the typed request set — enumerate it explicitly)"
    )]
    EmptySupervisorAuthority { profile_id: String },
    #[error(
        "supervisor profile {profile_id:?} repeats authority {authority:?} in its set \
         (SA-29 sets are sets, not multisets)"
    )]
    DuplicateSupervisorAuthority {
        profile_id: String,
        authority: zeroclaw_api::subagent_v1::SupervisorAuthority,
    },
    #[error(
        "v1 profile {profile_id:?} declares tools ({first:?}) outside the empty V1 child \
         catalog; V1 reasoning runs execute no tools"
    )]
    NonEmptyToolPolicy { profile_id: String, first: String },
    #[error(
        "profile {profile_id:?} revision {revision} is not increasing over the latest admitted revision"
    )]
    RevisionNotIncreasing { profile_id: String, revision: u32 },
}

/// The built-in default minimal Reasoning profile (SA-5's reference
/// profile): zero tools, bounded budget, structured report only.
pub const DEFAULT_REASONING_PROFILE_ID: &str = "default-reasoning-v1";

/// The built-in default Supervisor profile (vertical V3): EXACTLY the
/// ten typed SA-29 Tachi authorities, zero tools, no direct-execution
/// capability of any kind (SA-29/SA-30: the transitional trio is a
/// parent-kernel marking, never a Supervisor grant).
pub const DEFAULT_SUPERVISOR_PROFILE_ID: &str = "default-supervisor-v1";

impl SubAgentProfileRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry seeded with the default minimal Reasoning profile.
    pub fn with_default_reasoning_profile() -> Self {
        let mut registry = Self::new();
        registry
            .admit(Self::default_reasoning_profile())
            .expect("built-in default profile satisfies the frozen profile law");
        registry
    }

    #[must_use]
    pub fn default_reasoning_profile() -> SubAgentProfileV1 {
        let mut profile = SubAgentProfileV1 {
            profile_id: DEFAULT_REASONING_PROFILE_ID.to_string(),
            revision: 1,
            digest: String::new(),
            role: SubAgentRoleV1::Reasoning,
            model_policy: ModelPolicyV1 {
                provider_ref: String::new(),
                model: None,
                temperature: None,
            },
            tool_policy: SubAgentToolPolicyV1::default(),
            supervisor_authority_set: Vec::new(),
            context_policy: zeroclaw_api::subagent_v1::SubAgentContextPolicyV1 {
                allowed_classes: vec![
                    ContextClassV1::ObjectiveContext,
                    ContextClassV1::SourceRefs,
                    ContextClassV1::UserModelProjection,
                ],
                max_projection_bytes: 65_536,
            },
            privacy_policy: zeroclaw_api::subagent_v1::SubAgentPrivacyPolicyV1 {
                // Consistent with allowed_classes: public partitions
                // only. PrivateDyad is REDACTED unconditionally by
                // `projection` (SA-14.3 existence-blindness) and
                // AgentSoul is REFUSED unconditionally by
                // `projection_with_policy` (SA-15) — regardless of this
                // list.
                permitted_partitions: vec![
                    zeroclaw_api::companion::SourcePartition::UserModel,
                    zeroclaw_api::companion::SourcePartition::SharedLexicon,
                ],
            },
            budget: SubAgentBudgetV1::default(),
            recursion: zeroclaw_api::subagent_v1::SubAgentRecursionPolicyV1::NoLocalSpawn,
            output_schema: zeroclaw_api::subagent_v1::SubAgentOutputSchemaV1::StructuredReport,
        };
        profile.digest = profile.compute_digest();
        profile
    }

    /// The built-in default Supervisor profile (vertical V3): EXACTLY the
    /// ten typed SA-29 authorities, zero tools, structured report only.
    /// A Supervisor needing implementation work requests it as a typed
    /// `requested_parent_actions` entry — the PARENT submits through
    /// the TaskIntent bridge (the SA-29 set has no submit operation).
    #[must_use]
    pub fn default_supervisor_profile() -> SubAgentProfileV1 {
        use zeroclaw_api::subagent_v1::SupervisorAuthority as Authority;
        let mut profile = SubAgentProfileV1 {
            profile_id: DEFAULT_SUPERVISOR_PROFILE_ID.to_string(),
            revision: 1,
            digest: String::new(),
            role: SubAgentRoleV1::Supervisor,
            model_policy: ModelPolicyV1 {
                provider_ref: String::new(),
                model: None,
                temperature: None,
            },
            tool_policy: SubAgentToolPolicyV1::default(),
            supervisor_authority_set: vec![
                Authority::ObserveTask,
                Authority::ReadResultRefs,
                Authority::ProvideContext,
                Authority::RequestCorrection,
                Authority::RequestContinuation,
                Authority::RequestIndependentReview,
                Authority::RequestUserInput,
                Authority::RequestGracefulStop,
                Authority::RequestCancel,
                Authority::ProposeJudgment,
            ],
            context_policy: zeroclaw_api::subagent_v1::SubAgentContextPolicyV1 {
                allowed_classes: vec![
                    ContextClassV1::ObjectiveContext,
                    ContextClassV1::SourceRefs,
                    ContextClassV1::UserModelProjection,
                ],
                max_projection_bytes: 65_536,
            },
            privacy_policy: zeroclaw_api::subagent_v1::SubAgentPrivacyPolicyV1 {
                permitted_partitions: vec![
                    zeroclaw_api::companion::SourcePartition::UserModel,
                    zeroclaw_api::companion::SourcePartition::SharedLexicon,
                ],
            },
            budget: SubAgentBudgetV1::default(),
            recursion: zeroclaw_api::subagent_v1::SubAgentRecursionPolicyV1::NoLocalSpawn,
            output_schema: zeroclaw_api::subagent_v1::SubAgentOutputSchemaV1::StructuredReport,
        };
        profile.digest = profile.compute_digest();
        profile
    }

    /// Admit a profile revision. Validates the frozen law (SA-3/SA-4/
    /// SA-5/SA-29) and refuses otherwise. A capability change arrives
    /// here as a NEW revision with a NEW digest — never as a mutation of
    /// a live run (SA-4).
    pub fn admit(
        &mut self,
        mut profile: SubAgentProfileV1,
    ) -> Result<VersionedProfileRef, ProfileAdmissionError> {
        if let Err(mismatch) = profile.verify_digest() {
            return Err(ProfileAdmissionError::Digest(mismatch.to_string()));
        }
        // NOTE: an empty `model_policy.provider_ref` is legal for the
        // built-in default profile (it means "resolve from the parent's
        // provider at spawn time"); a spawn that can resolve no provider
        // at all fails at run time with a typed error.
        match profile.role {
            SubAgentRoleV1::Reasoning => {
                if !profile.supervisor_authority_set.is_empty() {
                    return Err(ProfileAdmissionError::SupervisorAuthorityOnReasoning {
                        profile_id: profile.profile_id.clone(),
                    });
                }
            }
            // SA-29 schema constraint (vertical V3): a Supervisor's
            // authority is the typed Tachi request set — the enum
            // guarantees every element is one of the ten; admission
            // additionally demands a non-empty, duplicate-free set. The
            // direct-execution law is structural: `tool_policy` is
            // checked below for EVERY role (the V1 catalog is empty, and
            // `shell`/`file_write`/`file_edit` cannot even be named —
            // `SubAgentToolNameV1::parse` refuses them, SA-30).
            SubAgentRoleV1::Supervisor => {
                if profile.supervisor_authority_set.is_empty() {
                    return Err(ProfileAdmissionError::EmptySupervisorAuthority {
                        profile_id: profile.profile_id.clone(),
                    });
                }
                let mut seen = std::collections::BTreeSet::new();
                for authority in &profile.supervisor_authority_set {
                    if !seen.insert(*authority) {
                        return Err(ProfileAdmissionError::DuplicateSupervisorAuthority {
                            profile_id: profile.profile_id.clone(),
                            authority: *authority,
                        });
                    }
                }
            }
        }
        if let Some(first) = profile.tool_policy.tools.first() {
            return Err(ProfileAdmissionError::NonEmptyToolPolicy {
                profile_id: profile.profile_id.clone(),
                first: first.as_str().to_string(),
            });
        }
        let latest = self
            .revisions
            .get(&profile.profile_id)
            .and_then(|list| list.last())
            .map(|p| p.revision)
            .unwrap_or(0);
        if profile.revision <= latest {
            return Err(ProfileAdmissionError::RevisionNotIncreasing {
                profile_id: profile.profile_id.clone(),
                revision: profile.revision,
            });
        }
        let vref = VersionedProfileRef {
            profile_id: profile.profile_id.clone(),
            revision: profile.revision,
            digest: profile.digest.clone(),
        };
        // The pinned digest is recomputed from the admitted content so
        // the registry's stored copy is canonical.
        profile.digest = profile.compute_digest();
        self.revisions
            .entry(profile.profile_id.clone())
            .or_default()
            .push(profile);
        Ok(vref)
    }

    /// Resolve an admitted `VersionedProfileRef`: profile id, revision,
    /// AND pinned digest must all match an admitted revision (SA-3).
    #[must_use]
    pub fn resolve(&self, vref: &VersionedProfileRef) -> Option<SubAgentProfileV1> {
        self.revisions
            .get(&vref.profile_id)?
            .iter()
            .find(|p| p.revision == vref.revision && p.digest == vref.digest)
            .cloned()
    }

    /// The latest admitted revision of a profile id.
    #[must_use]
    pub fn latest(&self, profile_id: &str) -> Option<SubAgentProfileV1> {
        self.revisions
            .get(profile_id)
            .and_then(|list| list.last())
            .cloned()
    }

    #[must_use]
    pub fn latest_ref(&self, profile_id: &str) -> Option<VersionedProfileRef> {
        self.latest(profile_id).map(|p| VersionedProfileRef {
            profile_id: p.profile_id,
            revision: p.revision,
            digest: p.digest,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Run admission (SA-3/SA-4/SA-12/SA-13)
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum RunAdmissionError {
    #[error("no admitted profile matches {profile_id:?} rev {revision} digest {digest}")]
    UnresolvedProfile {
        profile_id: String,
        revision: u32,
        digest: String,
    },
    #[error(
        "supervisor runs are not model-unit runs: profile {profile_id:?} must be driven \
         through supervisor_v1::SupervisorSessionV1 (SA-29), not the bounded reasoning \
         run constructor"
    )]
    SupervisorNotInV1 { profile_id: String },
    #[error(
        "local SubAgent-to-SubAgent spawn is denied (D1): spawning lineage is at depth {depth}, \
         only a parent at depth 0 may spawn a v1 child"
    )]
    DepthDenied { depth: u32 },
    #[error("declared tool list is not satisfiable by the v1 child catalog (SA-5/SA-7b): {reason}")]
    UnsatisfiableToolPolicy { reason: String },
}

/// A capability-change request against a LIVE run. Always refused — the
/// type has no success path (SA-4).
#[derive(Debug, thiserror::Error)]
#[error(
    "capability change refused for live run {run_ref:?}: the profile is immutable for the \
     run's lifetime; admit a new profile revision and materialize a new run"
)]
pub struct CapabilityChangeRefused {
    pub run_ref: String,
}

/// The admitted, immutable run (SA-3/SA-4). Constructible only via
/// [`SubAgentRunV1::from_admitted_profile`].
pub struct SubAgentRunV1 {
    profile: SubAgentProfileV1,
    pinned_digest: String,
    run_ref: SubAgentRunRef,
    control: Arc<Mutex<ControlInner>>,
    binding: OpaqueModelBinding,
}

impl std::fmt::Debug for SubAgentRunV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: identity and pinned digest only.
        f.debug_struct("SubAgentRunV1")
            .field("run_ref", &self.run_ref)
            .field("pinned_digest", &self.pinned_digest)
            .finish_non_exhaustive()
    }
}

impl SubAgentRunV1 {
    /// THE run constructor (SA-3): takes an admitted
    /// [`VersionedProfileRef`], never a raw profile. Refuses Supervisor
    /// profiles (V3 leaf) and any spawning lineage deeper than the
    /// parent (D1: children of V1 cannot spawn children — the refusal
    /// is at admission, typed, never tool prose).
    pub fn from_admitted_profile(
        registry: &SubAgentProfileRegistry,
        vref: &VersionedProfileRef,
        spawning_lineage: &LineageRef,
        binding: OpaqueModelBinding,
    ) -> Result<Self, RunAdmissionError> {
        let profile =
            registry
                .resolve(vref)
                .ok_or_else(|| RunAdmissionError::UnresolvedProfile {
                    profile_id: vref.profile_id.clone(),
                    revision: vref.revision,
                    digest: vref.digest.clone(),
                })?;
        if profile.role == SubAgentRoleV1::Supervisor {
            return Err(RunAdmissionError::SupervisorNotInV1 {
                profile_id: profile.profile_id,
            });
        }
        if spawning_lineage.depth() > 0 {
            return Err(RunAdmissionError::DepthDenied {
                depth: spawning_lineage.depth(),
            });
        }
        // ChildToolSet::from_profile is the admission gate for the
        // declared tool list (SA-5/SA-7b): an unsatisfiable list fails
        // HERE, before any run exists.
        ChildToolSet::from_profile(&profile).map_err(|e| {
            RunAdmissionError::UnsatisfiableToolPolicy {
                reason: e.to_string(),
            }
        })?;
        Ok(Self {
            pinned_digest: profile.digest.clone(),
            run_ref: SubAgentRunRef::from_opaque(format!("subagent-v1-{}", uuid::Uuid::new_v4())),
            profile,
            control: Arc::new(Mutex::new(ControlInner::default())),
            binding,
        })
    }

    /// Mid-run capability change: always refused for the live run; the
    /// pinned digest is untouched (SA-4). The widened capability is
    /// reachable only through a NEW admitted profile revision
    /// materialized as a new run.
    pub fn request_capability_change(
        &self,
        _widened: SubAgentToolPolicyV1,
    ) -> Result<(), CapabilityChangeRefused> {
        Err(CapabilityChangeRefused {
            run_ref: self.run_ref.as_str().to_string(),
        })
    }

    #[must_use]
    pub fn pinned_digest(&self) -> &str {
        &self.pinned_digest
    }

    /// The authority-minted, run-scoped identity (SA-13).
    #[must_use]
    pub fn run_ref(&self) -> &SubAgentRunRef {
        &self.run_ref
    }

    /// Parent-held control handle (SA-23).
    #[must_use]
    pub fn control_handle(&self) -> SubAgentControlHandle {
        SubAgentControlHandle {
            inner: Arc::clone(&self.control),
        }
    }

    /// Drive the bounded run. Returns the terminal
    /// [`SubAgentReportV1`] — the ONLY child→parent result channel
    /// (SA-21): there is no bare-text terminal path off this method.
    pub async fn execute(self, ctx: SubAgentExecutionContextV1) -> SubAgentReportV1 {
        self.execute_inner(ctx).await
    }

    async fn execute_inner(self, ctx: SubAgentExecutionContextV1) -> SubAgentReportV1 {
        let SubAgentExecutionContextV1 {
            objective,
            bundle,
            capabilities,
            report_channel,
            lineage,
            budget_meter,
        } = ctx;

        let failure = |status: SubAgentTerminalFact, summary: String, usage: SubAgentUsage| {
            SubAgentReportV1 {
                run_ref: self.run_ref.clone(),
                profile_ref: VersionedProfileRef {
                    profile_id: self.profile.profile_id.clone(),
                    revision: self.profile.revision,
                    digest: self.pinned_digest.clone(),
                },
                context_bundle_ref: bundle.bundle_id().to_string(),
                status,
                summary,
                findings: Vec::new(),
                evidence_refs: Vec::new(),
                uncertainty: Vec::new(),
                recommendations: Vec::new(),
                requested_parent_actions: Vec::new(),
                proposed_candidates: Vec::new(),
                usage,
            }
        };

        // Every terminal path sends the report on the channel (SA-21):
        // the early failure reports below use this local helper so none
        // of them can return without the send.
        let terminal = |report: SubAgentReportV1| async {
            let _ = report_channel
                .send(ReportChannelMessage::Report(Box::new(report.clone())))
                .await;
            report
        };

        // SA-18: the bundle digest is verified at admission AND before
        // use — mid-run bundle mutation is refused.
        if let Err(mismatch) = bundle.verify_digest() {
            return terminal(enforce_terminal_fact(
                failure(
                    SubAgentTerminalFact::Failed,
                    format!("context bundle digest refused: {mismatch}"),
                    budget_meter.usage(),
                ),
                false,
                false,
                false,
            ))
            .await;
        }

        // SA-5 defensive re-derivation: the materialized capability set
        // must still equal the pinned profile's declared list.
        let expected = match ChildToolSet::from_profile(&self.profile) {
            Ok(set) => set,
            Err(e) => {
                return terminal(enforce_terminal_fact(
                    failure(
                        SubAgentTerminalFact::Failed,
                        format!("capability set re-derivation failed: {e}"),
                        budget_meter.usage(),
                    ),
                    false,
                    false,
                    false,
                ))
                .await;
            }
        };
        if expected.names() != capabilities.names() {
            return terminal(enforce_terminal_fact(
                failure(
                    SubAgentTerminalFact::Failed,
                    "capability set does not equal the admitted profile's declared list"
                        .to_string(),
                    budget_meter.usage(),
                ),
                false,
                false,
                false,
            ))
            .await;
        }

        // SA-5/SA-14/SA-15/SA-18/SA-19: the policy-enforced projection
        // over the ADMITTED bundle. Admission (the FIRST privacy
        // boundary) already rejected Private-Dyad/AgentSoul-derived refs
        // before the run could hold the bundle; what meets the profile
        // HERE is narrowing only: classes not in
        // `context_policy.allowed_classes` are dropped, source refs on
        // partitions not in `privacy_policy.permitted_partitions` are
        // REFUSED with a typed error, and the projection size ceiling
        // enforces. Raw-side projection redaction remains as
        // defense-in-depth elsewhere; a bundle is content, never
        // authority (SA-18): this filter can only narrow, never widen.
        let projection = match bundle.projection_with_policy(
            &self.profile.context_policy.allowed_classes,
            &self.profile.privacy_policy.permitted_partitions,
            self.profile.context_policy.max_projection_bytes,
        ) {
            Ok(projection) => projection,
            Err(error) => {
                return terminal(enforce_terminal_fact(
                    failure(
                        SubAgentTerminalFact::Failed,
                        format!("context bundle refused by profile policy: {error}"),
                        budget_meter.usage(),
                    ),
                    false,
                    false,
                    false,
                ))
                .await;
            }
        };

        // Pre-unit control check (SA-23).
        let (graceful_seen, abort_seen) = {
            let control = self.control.lock();
            (control.graceful_stop_requested, control.abort_requested)
        };
        if abort_seen {
            let report = enforce_terminal_fact(
                failure(
                    SubAgentTerminalFact::Aborted,
                    "aborted before start".into(),
                    budget_meter.usage(),
                ),
                false,
                true,
                false,
            );
            let _ = report_channel
                .send(ReportChannelMessage::Report(Box::new(report.clone())))
                .await;
            return report;
        }
        if graceful_seen {
            let report = enforce_terminal_fact(
                failure(
                    SubAgentTerminalFact::Stopped,
                    "graceful stop before start".into(),
                    budget_meter.usage(),
                ),
                true,
                false,
                false,
            );
            let _ = report_channel
                .send(ReportChannelMessage::Report(Box::new(report.clone())))
                .await;
            return report;
        }

        // SA-27: the action ceiling enforces BEFORE the bounded unit.
        if !budget_meter.try_record_action() {
            let report = enforce_terminal_fact(
                failure(
                    SubAgentTerminalFact::TimedOut,
                    "action budget exhausted before the bounded unit".into(),
                    budget_meter.usage(),
                ),
                false,
                false,
                true,
            );
            let _ = report_channel
                .send(ReportChannelMessage::Report(Box::new(report.clone())))
                .await;
            return report;
        }

        let (system, user) = assemble_bounded_prompt(&objective, &projection, &lineage);
        let request = BoundedModelRequest {
            system,
            user,
            temperature: self.profile.model_policy.temperature,
        };

        // The ONE bounded unit. Abort interrupts it; graceful stop lets
        // it finish (SA-23).
        let remaining = budget_meter.remaining_time();
        let binding = self.binding;
        let unit = binding.complete(request);
        let outcome = tokio::select! {
            biased;
            _ = await_abort(Arc::clone(&self.control)) => {
                let report = enforce_terminal_fact(
                    failure(SubAgentTerminalFact::Aborted, "aborted during the bounded unit".into(), budget_meter.usage()),
                    false,
                    true,
                    false,
                );
                let _ = report_channel.send(ReportChannelMessage::Report(Box::new(report.clone()))).await;
                return report;
            }
            result = tokio::time::timeout(remaining, unit) => match result {
                Err(_) => {
                    let report = enforce_terminal_fact(
                        failure(SubAgentTerminalFact::TimedOut, "time budget exhausted during the bounded unit".into(), budget_meter.usage()),
                        false,
                        false,
                        true,
                    );
                    let _ = report_channel.send(ReportChannelMessage::Report(Box::new(report.clone()))).await;
                    return report;
                }
                Ok(Err(e)) => {
                    let report = enforce_terminal_fact(
                        failure(SubAgentTerminalFact::Failed, format!("model call failed: {e}"), budget_meter.usage()),
                        false,
                        false,
                        false,
                    );
                    let _ = report_channel.send(ReportChannelMessage::Report(Box::new(report.clone()))).await;
                    return report;
                }
                Ok(Ok(response)) => response,
            },
        };

        // SA-27: token usage is recorded and enforced over counted usage.
        let tokens_exceeded = !budget_meter.record_tokens(outcome.tokens_in, outcome.tokens_out);

        // Post-unit control check: graceful stop lets the current unit
        // finish — the parse happens, then the terminal fact is Stopped.
        let (graceful_seen, abort_seen) = {
            let control = self.control.lock();
            (control.graceful_stop_requested, control.abort_requested)
        };

        let core = parse_report_core(&outcome.text);
        let usage = budget_meter.usage();
        let report = match core {
            Ok(core) => {
                let mut r = failure(SubAgentTerminalFact::Completed, core.summary, usage);
                r.findings = core.findings;
                r.evidence_refs = core.evidence_refs;
                r.uncertainty = core.uncertainty;
                r.recommendations = core.recommendations;
                r.requested_parent_actions = core.requested_parent_actions;
                r.proposed_candidates = core.proposed_candidates;
                r
            }
            Err(e) => enforce_terminal_fact(
                failure(
                    SubAgentTerminalFact::Failed,
                    format!("report parse failed: {e}"),
                    usage,
                ),
                false,
                false,
                false,
            ),
        };

        let report = if tokens_exceeded {
            let mut timed_out = report;
            timed_out.status = SubAgentTerminalFact::TimedOut;
            timed_out.summary = format!(
                "token budget exhausted; last unit summary: {}",
                timed_out.summary
            );
            enforce_terminal_fact(timed_out, false, false, true)
        } else if abort_seen {
            let mut aborted = report;
            aborted.status = SubAgentTerminalFact::Aborted;
            aborted.summary = format!(
                "aborted after the bounded unit; summary: {}",
                aborted.summary
            );
            enforce_terminal_fact(aborted, false, true, false)
        } else if graceful_seen {
            let mut stopped = report;
            stopped.status = SubAgentTerminalFact::Stopped;
            stopped.summary = format!("gracefully stopped; summary: {}", stopped.summary);
            enforce_terminal_fact(stopped, true, false, false)
        } else {
            enforce_terminal_fact(report, false, false, false)
        };

        let _ = report_channel
            .send(ReportChannelMessage::Report(Box::new(report.clone())))
            .await;
        report
    }
}

/// The SA-23 terminal-fact guard, load-bearing: a terminal fact that
/// cannot prove its matching control event (or budget exhaustion) is
/// REFUSED — logged as an ERROR and downgraded to `failed`. No path can
/// write `stopped`/`aborted`/`timed_out` without its proof surviving
/// this function.
fn enforce_terminal_fact(
    report: SubAgentReportV1,
    graceful_stop_seen: bool,
    abort_seen: bool,
    budget_exhausted: bool,
) -> SubAgentReportV1 {
    let fact = report.status;
    match validate_terminal_transition(fact, graceful_stop_seen, abort_seen, budget_exhausted) {
        Ok(()) => report,
        Err(violation) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_ref": report.run_ref.as_str(),
                        "refused_fact": format!("{fact:?}"),
                        "violation": violation.to_string(),
                    })),
                "subagent_v1: terminal-fact guard refused an unproven terminal fact; downgraded to failed"
            );
            let mut refused = report;
            refused.status = SubAgentTerminalFact::Failed;
            refused.summary = format!(
                "terminal-fact guard refused {fact:?} ({violation}); downgraded to failed. Underlying summary: {}",
                refused.summary
            );
            refused
        }
    }
}

/// Wait until an abort event is raised (poll: the control state is a
/// plain flag; V1 has one bounded unit so the poll interval bounds
/// abort latency well under the unit's own timeout).
async fn await_abort(control: Arc<Mutex<ControlInner>>) {
    loop {
        if control.lock().abort_requested {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Execution context (SA-6: exactly six inputs)
// ─────────────────────────────────────────────────────────────────────────

/// Hard bound on the objective (bounded text, SA-6 input 1).
pub const MAX_OBJECTIVE_BYTES: usize = 8192;

#[derive(Debug, Clone)]
pub struct ObjectiveV1(String);

#[derive(Debug, thiserror::Error)]
#[error("objective exceeds the {MAX_OBJECTIVE_BYTES}-byte bound")]
pub struct ObjectiveTooLarge;

impl ObjectiveV1 {
    pub fn new(text: impl Into<String>) -> Result<Self, ObjectiveTooLarge> {
        let text = text.into();
        if text.len() > MAX_OBJECTIVE_BYTES {
            return Err(ObjectiveTooLarge);
        }
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The structured-report channel handle (SA-6 input 4): the child's ONLY
/// outbound surface. Carries typed mid-run events and the single
/// terminal report — never free prose (SA-21/SA-25).
#[derive(Clone)]
pub struct ReportChannelHandle {
    tx: tokio::sync::mpsc::Sender<ReportChannelMessage>,
}

impl ReportChannelHandle {
    #[must_use]
    pub fn new(tx: tokio::sync::mpsc::Sender<ReportChannelMessage>) -> Self {
        Self { tx }
    }

    async fn send(&self, message: ReportChannelMessage) -> Result<(), ReportChannelMessage> {
        self.tx.send(message).await.map_err(|error| error.0)
    }
}

/// The execution context: EXACTLY the six SA-6 inputs — objective,
/// ADMITTED bundle (the SA-14/SA-15 structural boundary: child
/// execution accepts only [`AdmittedContextBundleV1`], whose partition
/// enum cannot express Private Dyad or Agent Soul), capability list
/// (materialized from the admitted profile), structured-report
/// channel, lineage ref, shared budget meter — and nothing else. No
/// `Config`, no tool registry, no channel map, no memory backend
/// (compile-level signature test in this module's tests).
pub struct SubAgentExecutionContextV1 {
    objective: ObjectiveV1,
    bundle: AdmittedContextBundleV1,
    capabilities: ChildToolSet,
    report_channel: ReportChannelHandle,
    lineage: LineageRef,
    budget_meter: Arc<SubAgentBudgetMeter>,
}

impl SubAgentExecutionContextV1 {
    #[must_use]
    pub fn new(
        objective: ObjectiveV1,
        bundle: AdmittedContextBundleV1,
        capabilities: ChildToolSet,
        report_channel: ReportChannelHandle,
        lineage: LineageRef,
        budget_meter: Arc<SubAgentBudgetMeter>,
    ) -> Self {
        Self {
            objective,
            bundle,
            capabilities,
            report_channel,
            lineage,
            budget_meter,
        }
    }

    /// Typed inventory of everything the child context holds. The type
    /// itself is the negative-capability evidence: it has no field for a
    /// credential, a channel-map handle, a memory backend, or a
    /// workspace/filesystem root, because the context cannot hold any of
    /// them.
    #[must_use]
    pub fn inventory(&self) -> ContextInventory {
        ContextInventory {
            objective_bytes: self.objective.as_str().len(),
            bundle_id: self.bundle.bundle_id().to_string(),
            bundle_digest: self.bundle.digest().to_string(),
            capability_names: self
                .capabilities
                .names()
                .into_iter()
                .map(String::from)
                .collect(),
            lineage_root: self.lineage.root_ref().as_str().to_string(),
            lineage_depth: self.lineage.depth(),
            budget_max_actions: self.budget_meter.budget().max_actions,
            outbound_channel: "structured-report-only".to_string(),
        }
    }
}

/// What a child context contains (SA-6 inventory). No credential,
/// channel-map, memory, or workspace fields exist on this type. The
/// `Serialize` derive is deliberate: the serialized key set IS the
/// inventory — a test pins the exact field set, so adding a field
/// (e.g. a credential) becomes observable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContextInventory {
    pub objective_bytes: usize,
    pub bundle_id: String,
    pub bundle_digest: String,
    pub capability_names: Vec<String>,
    pub lineage_root: String,
    pub lineage_depth: u32,
    pub budget_max_actions: u32,
    pub outbound_channel: String,
}

// ─────────────────────────────────────────────────────────────────────────
// Prompt assembly (SA-16/SA-20)
// ─────────────────────────────────────────────────────────────────────────

/// Assemble the bounded child input from the objective and the bundle
/// projection ONLY. The parent transcript is structurally absent (no
/// parameter carries it — SA-16), and persona files reach a child only
/// via bundle refs, never the parent's (SA-20): this function performs
/// no file I/O at all.
fn assemble_bounded_prompt(
    objective: &ObjectiveV1,
    projection: &zeroclaw_api::subagent_v1::BundleProjection,
    lineage: &LineageRef,
) -> (String, String) {
    let system = format!(
        "You are a bounded reasoning SubAgent (v1). You receive one objective and an \
         immutable, digest-bound context snapshot. You have no tools. Respond with a \
         single JSON object and nothing else, matching exactly:\n\
         {{\"summary\": string, \"findings\": [{{\"finding_id\": string, \"statement\": \
         string, \"evidence_refs\": [string]}}], \"evidence_refs\": [string], \"uncertainty\": [{{\"uncertainty_id\": \
         string, \"topic_code\": string, \"impact\": string}}], \"recommendations\": \
         [{{\"recommendation_id\": string, \"statement\": string, \"evidence_refs\": \
         [string]}}], \"requested_parent_actions\": [{{\"action\": \
         \"ask_user\"|\"review_candidate\"|\"note\", \"subject_ref\": string}}], \
         \"proposed_candidates\": [{{\"candidate_id\": string, \"kind\": \
         \"ordinary_memory\"|\"user_model\"|\"agent_soul\"|\"skill\"|\"procedure\"|\
         \"private_dyad_derived\", \"content_digest\": string}}]}}\n\
         All arrays are optional and may be empty. Do not include chain-of-thought; \
         the report carries conclusions and evidence pointers only."
    );
    let mut user = format!("[Objective]\n{}\n", objective.as_str());
    user.push_str(&format!(
        "[Lineage]\nroot={} depth={}\n",
        lineage.root_ref().as_str(),
        lineage.depth()
    ));
    // Child-visible digest is the PROJECTION digest (existence-blind —
    // SA-14.3); the pinned full-bundle digest never reaches the child.
    user.push_str(&format!(
        "[ContextBundle {} projection_digest {}]\n",
        projection.bundle_id, projection.projection_digest
    ));
    if !projection.objective_context.is_empty() {
        user.push_str(&format!(
            "[ObjectiveContext]\n{}\n",
            projection.objective_context
        ));
    }
    for source in &projection.source_refs {
        user.push_str(&format!(
            "[SourceRef {} partition {} digest {}]\n",
            source.ref_id, source.partition, source.content_digest
        ));
    }
    for fact in &projection.applicable_user_model {
        user.push_str(&format!(
            "[ProjectedFact {} digest {}]\n",
            fact.fact_id, fact.statement_digest
        ));
    }
    for skill in &projection.skill_refs {
        user.push_str(&format!("[SkillRef {skill}]\n"));
    }
    for procedure in &projection.procedure_refs {
        user.push_str(&format!("[ProcedureRef {procedure}]\n"));
    }
    (system, user)
}

/// The JSON core the model returns; the run wraps it into the frozen
/// report type. `deny_unknown_fields` makes the parser itself reject
/// smuggled extras (a `chain_of_thought` field or any other unlisted
/// key fails the parse → the run ends `failed`), matching SA-22's
/// structural hygiene at every layer.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportCore {
    summary: String,
    #[serde(default)]
    findings: Vec<Finding>,
    #[serde(default)]
    evidence_refs: Vec<zeroclaw_api::subagent_v1::EvidenceRef>,
    #[serde(default)]
    uncertainty: Vec<zeroclaw_api::subagent_v1::UncertaintyItem>,
    #[serde(default)]
    recommendations: Vec<Recommendation>,
    #[serde(default)]
    requested_parent_actions: Vec<zeroclaw_api::subagent_v1::RequestedParentAction>,
    #[serde(default)]
    proposed_candidates: Vec<ProposedCandidate>,
}

fn parse_report_core(text: &str) -> Result<ReportCore, anyhow::Error> {
    let trimmed = text.trim();
    // Tolerate a surrounding code fence, nothing else.
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let core: ReportCore = serde_json::from_str(body)?;
    // The submit_task_intent action (and its typed payload) is the
    // SUPERVISOR session's vocabulary (SA-29 role-exclusive law): a
    // reasoning child's model output cannot carry it — the report is
    // malformed and the run fails, rather than smuggling a submit
    // request through the reasoning report channel.
    if core
        .requested_parent_actions
        .iter()
        .any(|action| action.action.requires_task_intent_payload())
    {
        anyhow::bail!(
            "requested_parent_actions carries a submit_task_intent action: that action is              supervisor-session vocabulary and cannot come from a reasoning child report"
        );
    }
    Ok(core)
}

// ─────────────────────────────────────────────────────────────────────────
// Candidate review queue (SA-17/SA-22)
// ─────────────────────────────────────────────────────────────────────────

/// Where a report's recommendations and proposed candidates land. The
/// queue's ONLY operations are route and discard: there is no apply —
/// `CandidateDisposition` has no applied variant, so no code path from
/// a report field to active authority state can exist here. Candidates
/// targeting KP-18 active-authority kinds route ONLY into the reviewed
/// promotion path (SA-17/SA-22 seam with the KP-18 knowledge split);
/// the Parent agent has no apply action for them.
pub struct SubAgentCandidateReviewQueue {
    entries: Mutex<Vec<QueuedCandidate>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCandidate {
    /// The run whose report proposed this candidate. Candidate ids are
    /// model-chosen labels: they are disambiguated by run, and a
    /// duplicate (run, candidate_id) is refused — a label collision
    /// can never route the wrong candidate.
    pub run_ref: SubAgentRunRef,
    pub candidate: ProposedCandidate,
    pub recommendation_ids: Vec<String>,
    pub disposition: CandidateDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateDisposition {
    /// Awaiting the Parent's disposition decision (route or discard).
    AwaitingParentDisposition,
    /// Routed into the reviewed promotion path. This is a ROUTING
    /// RECORD, not an application: nothing was applied to any authority
    /// state.
    RoutedToReviewedPromotion,
    Discarded,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewQueueError {
    #[error("candidate {candidate_id:?} is not in the queue")]
    UnknownCandidate { candidate_id: String },
    #[error(
        "candidate {candidate_id:?} already has the terminal disposition {disposition:?}; \
         dispositions are terminal and cannot be overwritten"
    )]
    AlreadyDecided {
        candidate_id: String,
        disposition: CandidateDisposition,
    },
    #[error(
        "candidate {candidate_id:?} targets KP-18 kind {kind:?}; its only path is the \
         reviewed promotion path (route_to_reviewed_promotion), never a direct apply"
    )]
    Kp18RequiresReviewedPromotion {
        candidate_id: String,
        kind: zeroclaw_api::subagent_v1::ProposedCandidateKind,
    },
    #[error(
        "candidate {candidate_id:?} targets KP-18 kind {kind:?} but carries no payload_ref \
         and/or provenance; a digest-only candidate cannot be routed into the reviewed \
         promotion path (it says a candidate exists, not WHAT it changes) — await a \
         payload-carrying revision"
    )]
    DigestOnlyCandidateNotRoutable {
        candidate_id: String,
        kind: zeroclaw_api::subagent_v1::ProposedCandidateKind,
    },
}

impl SubAgentCandidateReviewQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Land a report's recommendations and candidates in the queue
    /// (SA-22: they land in review queues; they never auto-ratify).
    /// Returns the number of candidates queued.
    pub fn receive(&self, report: &SubAgentReportV1) -> usize {
        let mut entries = self.entries.lock();
        let mut count = 0;
        for candidate in &report.proposed_candidates {
            let duplicate = entries.iter().any(|entry| {
                entry.run_ref == report.run_ref
                    && entry.candidate.candidate_id == candidate.candidate_id
            });
            if duplicate {
                // A model-chosen label colliding inside one run is a
                // malformed report: the duplicate is refused, not
                // silently accepted where it could mis-route.
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "run_ref": report.run_ref.as_str(),
                            "candidate_id": candidate.candidate_id,
                        })),
                    "subagent_v1: duplicate candidate id within one run refused by the review queue"
                );
                continue;
            }
            entries.push(QueuedCandidate {
                run_ref: report.run_ref.clone(),
                candidate: candidate.clone(),
                recommendation_ids: report
                    .recommendations
                    .iter()
                    .map(|r| r.recommendation_id.clone())
                    .collect(),
                disposition: CandidateDisposition::AwaitingParentDisposition,
            });
            count += 1;
        }
        count
    }

    /// Route a candidate into the reviewed promotion path. Returns the
    /// routing record; applies nothing. The only disposition available
    /// to any candidate — KP-18 kinds included — is this routing or
    /// discard. A KP-18 candidate that carries no payload_ref and/or no
    /// provenance is REFUSED here (the P2-caveat law from vertical V3):
    /// digest-only candidates have no promotable substance, and the
    /// refusal keeps them queued for a payload-carrying revision instead
    /// of silently dropping or promoting them.
    pub fn route_to_reviewed_promotion(
        &self,
        run_ref: &SubAgentRunRef,
        candidate_id: &str,
    ) -> Result<CandidateRoutingRecord, ReviewQueueError> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.run_ref == *run_ref && e.candidate.candidate_id == candidate_id)
            .ok_or_else(|| ReviewQueueError::UnknownCandidate {
                candidate_id: candidate_id.to_string(),
            })?;
        // Dispositions are TERMINAL: a decided candidate cannot be
        // re-routed or discarded (no state overwrite, no flapping).
        if entry.disposition != CandidateDisposition::AwaitingParentDisposition {
            return Err(ReviewQueueError::AlreadyDecided {
                candidate_id: candidate_id.to_string(),
                disposition: entry.disposition,
            });
        }
        if entry.candidate.kind.requires_reviewed_promotion() && !entry.candidate.is_substantiated()
        {
            return Err(ReviewQueueError::DigestOnlyCandidateNotRoutable {
                candidate_id: candidate_id.to_string(),
                kind: entry.candidate.kind,
            });
        }
        entry.disposition = CandidateDisposition::RoutedToReviewedPromotion;
        Ok(CandidateRoutingRecord {
            run_ref: run_ref.clone(),
            candidate_id: candidate_id.to_string(),
            kind: entry.candidate.kind,
            routed_to: "reviewed_promotion_path".to_string(),
            content_digest: entry.candidate.content_digest.clone(),
            payload_ref: entry.candidate.payload_ref.clone(),
            provenance: entry.candidate.provenance.clone(),
        })
    }

    /// Discard a candidate. A disposition decision, not an application.
    pub fn discard(
        &self,
        run_ref: &SubAgentRunRef,
        candidate_id: &str,
    ) -> Result<(), ReviewQueueError> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.run_ref == *run_ref && e.candidate.candidate_id == candidate_id)
            .ok_or_else(|| ReviewQueueError::UnknownCandidate {
                candidate_id: candidate_id.to_string(),
            })?;
        if entry.disposition != CandidateDisposition::AwaitingParentDisposition {
            return Err(ReviewQueueError::AlreadyDecided {
                candidate_id: candidate_id.to_string(),
                disposition: entry.disposition,
            });
        }
        entry.disposition = CandidateDisposition::Discarded;
        Ok(())
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<QueuedCandidate> {
        self.entries.lock().clone()
    }
}

impl Default for SubAgentCandidateReviewQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A routing record: proof the candidate was sent to the reviewed
/// promotion path. Carries no authority and applies nothing. From
/// vertical V3 on it also carries the candidate's payload reference and
/// provenance (the promotion substance — what the reviewed path is to
/// act on), when the candidate provides them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRoutingRecord {
    pub run_ref: SubAgentRunRef,
    pub candidate_id: String,
    pub kind: zeroclaw_api::subagent_v1::ProposedCandidateKind,
    pub routed_to: String,
    /// The candidate's content digest — carried so the reviewed path can
    /// bind the routed payload to the proposed content.
    pub content_digest: String,
    pub payload_ref: Option<String>,
    pub provenance: Option<zeroclaw_api::subagent_v1::CandidateProvenance>,
}

// ─────────────────────────────────────────────────────────────────────────
// Parent-side spawn tool
// ─────────────────────────────────────────────────────────────────────────

/// The Parent's bounded reasoning-child spawn tool. This is a PARENT
/// capability (D1: `Parent → local SubAgent` allowed): the child it
/// spawns is a V1 reasoning run with no tools, an immutable bundle, and
/// a structured report. Children cannot use this tool — run admission
/// refuses any spawning lineage at depth > 0 (SA-12).
pub struct ReasoningSubagentTool {
    config: Arc<Config>,
    parent_alias: String,
    security: Arc<zeroclaw_config::policy::SecurityPolicy>,
    lineage: Option<LineageRef>,
    registry: Mutex<SubAgentProfileRegistry>,
    review_queue: Arc<SubAgentCandidateReviewQueue>,
    /// Host-side model-resolver override. When set, the host resolves
    /// the child's bounded completions through this resolver instead of
    /// the operator provider configuration (used by embedders and
    /// tests). The child context is unaffected: it still sees only the
    /// opaque binding.
    model_resolver_override: Option<Arc<dyn ModelAccessResolver>>,
}

impl ReasoningSubagentTool {
    pub const NAME: &'static str = "reasoning_subagent";

    pub fn new(
        config: Arc<Config>,
        parent_alias: impl Into<String>,
        security: Arc<zeroclaw_config::policy::SecurityPolicy>,
    ) -> Self {
        Self {
            config,
            parent_alias: parent_alias.into(),
            security,
            lineage: None,
            registry: Mutex::new(SubAgentProfileRegistry::with_default_reasoning_profile()),
            review_queue: Arc::new(SubAgentCandidateReviewQueue::new()),
            model_resolver_override: None,
        }
    }

    /// Carry the spawning context's lineage (SA-9): the tool refuses to
    /// spawn when its own context is already a child (admission-side
    /// D1 enforcement via `SubAgentRunV1::from_admitted_profile`).
    #[must_use]
    pub fn with_lineage(mut self, lineage: Option<LineageRef>) -> Self {
        self.lineage = lineage;
        self
    }

    /// Host-side model-resolver override (embedder/test seam). The
    /// opaque-binding boundary is unchanged.
    #[must_use]
    pub fn with_model_resolver(mut self, resolver: Arc<dyn ModelAccessResolver>) -> Self {
        self.model_resolver_override = Some(resolver);
        self
    }

    #[must_use]
    pub fn review_queue(&self) -> Arc<SubAgentCandidateReviewQueue> {
        Arc::clone(&self.review_queue)
    }

    fn effective_lineage(&self) -> LineageRef {
        let own = self.lineage.clone().unwrap_or_else(|| {
            LineageRef::new_root(ParentRunRef::from_opaque(format!(
                "agent:{}",
                self.parent_alias
            )))
        });
        // The ambient scope wins when deeper: a bounded-delegate child
        // calling this inherited tool runs in the CHILD's context (D1 —
        // run admission below refuses depth > 0).
        match ambient_lineage() {
            Some(ambient) if ambient.depth() > own.depth() => ambient,
            _ => own,
        }
    }

    /// Run one bounded child. `meter_override` is None on the production
    /// path (the shared meter is derived from the admitted profile
    /// revision); tests may pass an explicit shared meter to prove the
    /// SA-8 sharing semantics.
    async fn run_child(&self, objective: &str) -> Result<SubAgentReportV1, String> {
        self.run_child_with(objective, None).await
    }

    pub(crate) async fn run_child_with(
        &self,
        objective: &str,
        meter_override: Option<Arc<SubAgentBudgetMeter>>,
    ) -> Result<SubAgentReportV1, String> {
        let lineage = self.effective_lineage();
        let (profile, vref) = {
            // Scoped guard: lexically bounded, released before any await.
            let registry_guard = self.registry.lock();
            let profile = registry_guard
                .latest(DEFAULT_REASONING_PROFILE_ID)
                .ok_or_else(|| "default reasoning profile is not admitted".to_string())?;
            let vref = VersionedProfileRef {
                profile_id: profile.profile_id.clone(),
                revision: profile.revision,
                digest: profile.digest.clone(),
            };
            (profile, vref)
        };

        // SA-8, per-run scope: a FRESH meter is minted for THIS run and
        // shared between this parent-side admission and the child
        // execution (the same `Arc` crosses the SA-6 boundary as the
        // budget input); it is discarded at the run's terminal. There
        // is deliberately NO process-lifetime meter cache keyed by
        // profile/revision: the previous cache held one non-resetting
        // `Instant::now()` start for the tool's whole lifetime, so the
        // first run consumed the 120s window and every later turn was
        // permanently rejected until restart — a single-run ceiling
        // moonlighting as aggregate quota. Aggregate quotas
        // (per-ParentRun spawn counts, hourly token caps) are a
        // separate future `ParentRunQuota`/`RateBudget` concept and
        // must never be implemented by stretching a single-run meter.
        // Hosts that DO want to share one meter across spawns use
        // `meter_override` explicitly (that is also the SA-8 sharing
        // seam the action-ceiling test proves).
        let meter = match meter_override {
            Some(meter) => meter,
            None => Arc::new(SubAgentBudgetMeter::new(profile.budget)),
        };
        if meter.exhausted() {
            return Err(
                "reasoning_subagent: the budget meter for this run is exhausted before \
                 start (shared-override meters only); a fresh production run mints a \
                 fresh meter"
                    .to_string(),
            );
        }

        // The model binding is host-resolved from the profile's
        // model_policy at use time; when the profile names no provider,
        // the parent's own resolved provider is used (still opaque to
        // the child).
        let binding = match self.model_resolver_override.clone() {
            Some(resolver) => OpaqueModelBinding::new(resolver),
            None => {
                let policy = if profile.model_policy.provider_ref.trim().is_empty() {
                    let (provider_ref, model) = self
                        .config
                        .resolved_model_provider_for_agent(&self.parent_alias)
                        .map(|(family, alias, entry)| {
                            (format!("{family}.{alias}"), entry.model.clone())
                        })
                        .unwrap_or_default();
                    ModelPolicyV1 {
                        provider_ref,
                        model,
                        temperature: None,
                    }
                } else {
                    profile.model_policy.clone()
                };
                if policy.provider_ref.trim().is_empty() {
                    return Err(
                        "no model provider resolvable for the reasoning child (neither the \
                         profile's model_policy nor the parent's provider configuration)"
                            .to_string(),
                    );
                }
                OpaqueModelBinding::new(Arc::new(ConfigModelAccessResolver::new(
                    Arc::clone(&self.config),
                    policy,
                )))
            }
        };

        // SA-18/SA-16: a minimal, digest-bound bundle. The objective is
        // the context; the parent transcript is excluded by default
        // (auditable exclusion, SA-19); no memory read happens on the
        // V1 path (D2 snapshot-only).
        let objective = ObjectiveV1::new(objective).map_err(|e| e.to_string())?;
        let mut bundle = ContextBundleV1 {
            bundle_id: format!("bundle-{}", uuid::Uuid::new_v4()),
            revision: 1,
            digest: String::new(),
            parent_ref: ParentRunRef::from_opaque(lineage.root_ref().as_str()),
            objective_context: objective.as_str().to_string(),
            source_refs: Vec::new(),
            applicable_user_model: Vec::new(),
            skill_refs: Vec::new(),
            procedure_refs: Vec::new(),
            explicit_exclusions: vec![ContextClassV1::ParentTranscript],
            redaction_policy: BundleRedactionPolicy::default(),
        };
        bundle.digest = bundle.compute_digest();

        // The FIRST privacy boundary (SA-14/SA-15): reject — never
        // filter — Private-Dyad/AgentSoul-derived refs before the run
        // exists. The V1 path builds an empty source-ref list, so this
        // holds trivially today; the boundary exists so a future
        // capture sweep cannot regress to redaction-only protection.
        let bundle = bundle.admit().map_err(|error| {
            format!("context bundle refused at the admitted-bundle boundary: {error}")
        })?;

        let capabilities = ChildToolSet::from_profile(&profile)
            .map_err(|e| format!("child tool set admission failed: {e}"))?;

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        // Scoped guard: the registry lock is released before the child
        // run is driven (nothing holds a lock across an await here).
        let run = {
            let registry = self.registry.lock();
            SubAgentRunV1::from_admitted_profile(&registry, &vref, &lineage, binding)
                .map_err(|e| format!("run admission refused: {e}"))?
        };
        let ctx = SubAgentExecutionContextV1::new(
            objective,
            bundle,
            capabilities,
            ReportChannelHandle::new(tx),
            lineage.child(),
            meter,
        );

        let report = run.execute(ctx).await;
        // Drain any mid-run events so the typed channel is fully
        // observed by the parent side (none are emitted in V1's
        // single-unit runs; the surface exists for later units).
        while let Ok(_message) = rx.try_recv() {}
        let queued = self.review_queue.receive(&report);
        if queued > 0 {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Load)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({
                        "run_ref": report.run_ref.as_str(),
                        "queued_candidates": queued,
                    })),
                "subagent_v1: report candidates landed in the review queue"
            );
        }
        Ok(report)
    }
}

#[async_trait]
impl Tool for ReasoningSubagentTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run one bounded reasoning SubAgent against a focused objective. The child \
         receives only the objective and a digest-bound context snapshot — no tools, \
         no conversation history, no memory — and returns a structured report \
         (summary, findings, evidence pointers, uncertainty, recommendations, \
         candidate changes for parent disposition). Use for focused analysis that \
         should not pollute this agent's history."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The bounded, self-contained question or analysis task. The child does not see this conversation's history."
                }
            },
            "required": ["objective"]
        })
    }

    /// The structured output IS the child's `SubAgentReportV1` (SA-21):
    /// the report travels as `ToolOutput.data`; the display text is
    /// presentation derived from it, never the contract.
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "description": "SubAgentReportV1 — the ONLY child→parent result channel",
            "required": ["run_ref", "status", "summary", "usage"],
            "properties": {
                "run_ref": {"type": "string"},
                "profile_ref": {"type": "object"},
                "context_bundle_ref": {"type": "string"},
                "status": {
                    "type": "string",
                    "enum": ["stopped", "aborted", "timed_out", "completed", "failed"]
                },
                "summary": {"type": "string"},
                "findings": {"type": "array"},
                "evidence_refs": {"type": "array"},
                "uncertainty": {"type": "array"},
                "recommendations": {"type": "array"},
                "requested_parent_actions": {"type": "array"},
                "proposed_candidates": {"type": "array"},
                "usage": {
                    "type": "object",
                    "required": ["elapsed_ms", "tokens_in", "tokens_out", "actions"]
                }
            },
            "additionalProperties": false
        }))
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Card / risk-profile self-check, mirroring spawn_subagent's gate:
        // the tool must be granted by the governing card or listed in the
        // risk profile's allowed_tools (when a list exists), and not
        // excluded.
        if let Some(card) = self.config.card_for_agent(&self.parent_alias) {
            let granted = card.grants.tools.iter().any(|g| g.tool == Self::NAME);
            if !granted {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "reasoning_subagent: refused — card governing agent '{}' does not grant reasoning_subagent",
                        self.parent_alias
                    )),
                });
            }
        } else if let Some(rp) = self.config.risk_profile_for_agent(&self.parent_alias) {
            let excluded = rp.excluded_tools.iter().any(|t| t == Self::NAME);
            let allowed_when_listed = match &rp.allowed_tools {
                None => true,
                Some(tools) => tools.iter().any(|t| t == Self::NAME),
            };
            if excluded || !allowed_when_listed {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "reasoning_subagent: refused — agent '{}' risk_profile does not list reasoning_subagent in allowed_tools",
                        self.parent_alias
                    )),
                });
            }
        }

        if let Err(error) = self
            .security
            .enforce_tool_operation(crate::security::policy::ToolOperation::Act, Self::NAME)
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        let objective = match args
            .get("objective")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => value.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing or empty 'objective' parameter".into()),
                });
            }
        };

        match self.run_child(&objective).await {
            Ok(report) => {
                let mut summary = format!(
                    "reasoning_subagent {} [{}]\nsummary: {}",
                    report.run_ref.as_str(),
                    serde_json::to_value(report.status)
                        .unwrap_or_default()
                        .as_str()
                        .unwrap_or("unknown"),
                    report.summary,
                );
                for finding in &report.findings {
                    summary.push_str(&format!(
                        "\nfinding {}: {}",
                        finding.finding_id, finding.statement
                    ));
                }
                if !report.uncertainty.is_empty() {
                    summary.push_str(&format!(
                        "\nuncertainty items: {}",
                        report
                            .uncertainty
                            .iter()
                            .map(|u| u.uncertainty_id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !report.proposed_candidates.is_empty() {
                    summary.push_str(&format!(
                        "\ncandidates queued for parent disposition: {}",
                        report
                            .proposed_candidates
                            .iter()
                            .map(|c| c.candidate_id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let success = report.status == SubAgentTerminalFact::Completed;
                // SA-21: the structured report IS the result — the full
                // SubAgentReportV1 travels as `ToolOutput.data`, AND the
                // rendered text (the only part the parent model reads —
                // `tool_execution` shows the LLM `output` alone) carries
                // the report JSON verbatim, so the structured result
                // survives end to end. The prose header is presentation
                // derived from the report, never the contract.
                let report_json = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
                let json_text = serde_json::to_string_pretty(&report_json)
                    .unwrap_or_else(|_| report_json.to_string());
                let rendered = format!("{summary}\n\n[SubAgentReportV1]\n{json_text}");
                // SA-21 end to end: on a non-Completed terminal fact the
                // common dispatcher discards `output`/`data` and shows
                // the parent model only the error string — so the error
                // string carries the structured report too (subject to
                // the engine's uniform presentation truncation, the
                // same policy every tool output follows).
                let error = (!success).then(|| {
                    format!(
                        "child ended {:?}\n[SubAgentReportV1]\n{json_text}",
                        report.status
                    )
                });
                Ok(ToolResult {
                    success,
                    output: ToolOutput::json_with_text(report_json, rendered),
                    error,
                })
            }
            Err(error) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            }),
        }
    }
}

#[cfg(test)]
mod tests;
