//! The V3 Supervisor session: a typed state machine that drives
//! implementation plus fresh-context independent review through Tachi —
//! the runtime productization of the owner's human loop (implementation
//! → adversarial review → correction → re-review → judgment → report).
//!
//! ```text
//! Parent
//!   → SupervisorSessionV1                 admitted Supervisor profile (SA-3)
//!   → plan_implementation_task            typed `requested_parent_actions`
//!                                        (submit_task_intent payload; SA-29:
//!                                        the set has no submit op — the
//!                                        PARENT performs the initial submit)
//!   → attach_implementation_task          Tachi-minted TaskRef handed back
//!   → observe / collect_result            ObserveTask / ReadResultRefs
//!   → request_independent_review          TB-11 mapping: NEW review task on a
//!                                        distinct Task/context lineage, its
//!                                        independence class recorded (TB-17)
//!   → request_correction … re-review      at least one correction cycle
//!   → propose_judgment                    receipt-bound proposal — NEVER
//!                                        canonical adjudication truth (SA-29,
//!                                        KP-21, TB-13)
//!   → conclude                            SupervisorReport = SubAgentReportV1
//!                                        → Parent (SA-21, the ONLY channel)
//! ```
//!
//! Authority laws encoded here:
//!
//! - **No direct-execution authority** (SA-29/SA-30, N-R2 boundary 2):
//!   the session type holds no `Config`, no tool registry, no filesystem
//!   root, no shell/process handle, and no spawn surface — its ONLY
//!   outbound surface is the gated bridge client plus the structured
//!   report. The field set is pinned by [`SupervisorInventory`].
//! - **Child/review work through Tachi only** (SA-12 = D1): there is no
//!   local spawn path of any kind in this module (source-scan test).
//! - **Submit is parent-only for implementation** (SA-29 role-exclusive
//!   law): `plan_implementation_task` composes the intent and RETURNS it
//!   as a typed parent action; the session's own submit surface is the
//!   `RequestIndependentReview` mapping, which can only mint
//!   `reasoning_review` review tasks (capability forced), never
//!   implementation tasks.
//! - **Continuation ≠ independent review / same session ≠ independent
//!   review** (TB-17): the only constructor of
//!   [`ReviewLineageRecord`] is [`Self::request_independent_review`],
//!   which refuses non-independence-marked classes, mints a fresh
//!   context bundle ref distinct from the implementation task's, and
//!   records the class; `ReviewLineageRecord::satisfies` additionally
//!   requires the distinct lineage. A continuation receipt can never
//!   construct a record (structural) and `SameSessionContinuation`
//!   fails `is_independence_marked` (typed).
//! - **Worker success ≠ adjudication** (TB-13/TB-18):
//!   [`Self::propose_judgment`] reads the verdict EXCLUSIVELY from the
//!   adjudication dimension of the collected projection; the worker's
//!   terminal classification is carried as an observed FACT, explicitly
//!   not the verdict.
//! - **Supervisor judgment ≠ canonical eval truth** (SA-29, KP-21):
//!   `propose_judgment` performs NO bridge write of any kind — the
//!   proposal is run-scoped content bound to receipts; canonical
//!   adjudication state stays Tachi's (test observes it unchanged
//!   across a proposal).
//! - **Nothing durable** (SA-26/TB-22): every field of the session is
//!   run-scoped; this module owns no DDL, opens no database, and writes
//!   no files (integration test mirrors `subagent_v1`'s).

use std::collections::BTreeSet;
use std::sync::Arc;

use zeroclaw_api::subagent_v1::{
    LineageRef, ParentActionKind, RequestedParentAction, SubAgentProfileV1, SubAgentRoleV1,
    SubAgentRunRef as RunRef, SubAgentTerminalFact, SupervisorAuthority, TaskIntentSubmitRequest,
    VersionedProfileRef,
};
use zeroclaw_api::taskintent::{
    BoundedText, Capability, CapabilityRequest, IndependenceClass, InterventionReceipt, RequestId,
    RequesterRef, TaskConstraint, TaskRef,
};

use crate::subagent_v1::{SubAgentBudgetMeter, SubAgentProfileRegistry};
use crate::tachi_bridge::{
    ComposeRejection, ResultProjectionView, StructuralIntentContext, SubmitReceipt,
    SupervisorIntervention, SupervisorInterventionError, TachiBridgeClient, TaskIntentInputs,
    TaskSnapshotView, compose_intent,
};

#[cfg(test)]
mod tests;

/// Run-scoped record of one independent-review task (TB-17): the review's
/// OWN task identity, what it reviews, its recorded independence class,
/// and its context lineage. The ONLY constructor is
/// [`SupervisorSessionV1::request_independent_review`] — there is no
/// other way to spell review lineage, so a continuation (or a
/// same-session follow-up) can never masquerade as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLineageRecord {
    /// The review task's Tachi-minted identity.
    pub review_task: TaskRef,
    /// The task being reviewed.
    pub review_of: TaskRef,
    /// The review's recorded independence class (TB-17: recorded
    /// explicitly, never assumed from harness identity).
    pub independence_class: IndependenceClass,
    /// The review task's context bundle ref (fresh lineage).
    pub context_bundle_ref: String,
    /// The implementation task's context bundle ref — carried so lineage
    /// distinctness is checkable from the record itself.
    pub implementation_bundle_ref: String,
    /// The TB-7 request id the review was submitted under.
    pub request_id: String,
}

impl ReviewLineageRecord {
    /// Whether this review satisfies an independence-marked requirement:
    /// the recorded class must satisfy the requirement under the frozen
    /// TB-17 law AND the context lineage must be distinct from the
    /// implementation task's (a review sharing the implementation's
    /// context bundle is not independent, whatever its class label
    /// says).
    #[must_use]
    pub fn satisfies(&self, required: IndependenceClass) -> bool {
        self.context_bundle_ref != self.implementation_bundle_ref
            && self.independence_class.satisfies_requirement(required)
    }
}

/// A receipt-bound judgment proposal (SA-29 `ProposeJudgment`): run-scoped
/// INTERPRETATION over Tachi's adjudication truth — never canonical eval
/// state (KP-21). Every field is derived from a receipt (task ref, result
/// revision, projected labels); producing a proposal performs no bridge
/// write, so it cannot be a lifecycle transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgmentProposalV1 {
    pub proposal_id: String,
    /// The task the proposal interprets.
    pub task_ref: TaskRef,
    /// The Tachi-minted result revision the proposal is bound to.
    pub result_revision: u64,
    /// The ADJUDICATION-dimension label observed at that revision — the
    /// only verdict source (TB-13).
    pub adjudication_observed: String,
    /// The worker's terminal classification at that revision — an
    /// observed FACT, explicitly NOT the verdict (TB-13/TB-18).
    pub terminal_classification_observed: String,
    /// The supervisor's proposed interpretation, stated AS
    /// interpretation (the Parent presents; Tachi adjudicates).
    pub proposed_interpretation: String,
    /// Evidence refs the projection carried.
    pub evidence_refs: Vec<String>,
}

/// The session's phase machine. Typed transitions only; out-of-order
/// calls are refused with [`SupervisorFlowError::WrongPhase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorPhase {
    /// Planning: the implementation intent has not been handed to the
    /// Parent yet.
    Planning,
    /// Supervising: the implementation TaskRef is attached; observe /
    /// review / correction operate on it.
    Supervising,
    /// Concluded: the terminal report has been assembled.
    Concluded,
}

/// Typed admission failure for a supervisor session.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorAdmissionError {
    #[error("no admitted profile matches {profile_id:?} rev {revision} digest {digest}")]
    UnresolvedProfile {
        profile_id: String,
        revision: u32,
        digest: String,
    },
    #[error(
        "profile {profile_id:?} is not a Supervisor profile; supervisor sessions admit \
         Supervisor-role profiles only (SA-3)"
    )]
    RoleNotSupervisor { profile_id: String },
    #[error(
        "supervisor session refused: spawning lineage is at depth {depth} (D1: only a parent \
         at depth 0 may run a supervisor session)"
    )]
    DepthDenied { depth: u32 },
}

/// Typed flow failure inside a live session.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorFlowError {
    #[error("supervisor flow refused in phase {phase:?}: {reason}")]
    WrongPhase {
        phase: SupervisorPhase,
        reason: &'static str,
    },
    #[error("no implementation task attached yet")]
    NoImplementationTask,
    #[error(
        "independence class {class:?} is not independence-marked: continuation and \
         deterministic checks can never satisfy an independent-review requirement (TB-17)"
    )]
    ClassNotIndependenceMarked { class: IndependenceClass },
    #[error(
        "the session could not mint a fresh review context bundle distinct from the \
         implementation task's (lineage distinctness is mandatory, TB-17)"
    )]
    FreshBundleMintFailed,
    #[error("compose rejected the intent: {0}")]
    Compose(#[from] ComposeRejection),
    #[error(
        "review task submission was not admitted by the host: {receipt:?} (TB-20: no \
         local fallback, no second task is fabricated)"
    )]
    ReviewSubmitRefused { receipt: SubmitReceipt },
    #[error("authority {authority:?} is not granted by this supervisor profile (SA-29)")]
    AuthorityNotGranted { authority: SupervisorAuthority },
    #[error(
        "session-generated value of {len} bytes exceeded the wire text bound (the wire cap \
         is a typed law, not a truncation site)"
    )]
    InternalBounds { len: usize },
    #[error("session budget exhausted (SA-27): {needed}")]
    BudgetExhausted { needed: &'static str },
    #[error("collect failed for {task_ref}: {error}")]
    CollectFailed {
        task_ref: TaskRef,
        error: crate::tachi_bridge::BridgeQueryError,
    },
    #[error("observe failed for {task_ref}: {error}")]
    ObserveFailed {
        task_ref: TaskRef,
        error: crate::tachi_bridge::BridgeQueryError,
    },
    #[error("intervention failed: {0}")]
    Intervention(#[from] SupervisorInterventionError),
}

/// The supervisor session. Constructed ONLY from an admitted
/// Supervisor-role [`VersionedProfileRef`] (SA-3); holds NO execution
/// capability of any kind (see the module authority laws).
pub struct SupervisorSessionV1 {
    profile: SubAgentProfileV1,
    pinned_digest: String,
    run_ref: RunRef,
    /// The `subrun:`-namespaced wire identity this session's review
    /// submissions carry (TB-14 `supervisor_ref`).
    supervisor_wire_ref: zeroclaw_api::taskintent::SubAgentRunRef,
    granted: BTreeSet<SupervisorAuthority>,
    requester: RequesterRef,
    parent_ref: Option<zeroclaw_api::taskintent::ParentRunRef>,
    client: TachiBridgeClient,
    phase: SupervisorPhase,
    planned: bool,
    implementation_task: Option<TaskRef>,
    implementation_bundle_ref: Option<String>,
    implementation_intent_digest: Option<String>,
    reviews: Vec<ReviewLineageRecord>,
    proposals: Vec<JudgmentProposalV1>,
    meter: Arc<SubAgentBudgetMeter>,
    bridge_ops: u32,
    next_request_seq: u32,
}

impl std::fmt::Debug for SupervisorSessionV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: identity and pinned digest only — no handles.
        f.debug_struct("SupervisorSessionV1")
            .field("run_ref", &self.run_ref)
            .field("pinned_digest", &self.pinned_digest)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl SupervisorSessionV1 {
    /// THE session constructor (SA-3): an admitted Supervisor profile,
    /// a spawning lineage at depth 0, and the bridge client the PARENT
    /// binds. No model binding is taken — supervisor sessions run no
    /// model units; their steps are typed state transitions.
    pub fn from_admitted_profile(
        registry: &SubAgentProfileRegistry,
        vref: &VersionedProfileRef,
        spawning_lineage: &LineageRef,
        client: TachiBridgeClient,
        requester: RequesterRef,
        parent_ref: Option<zeroclaw_api::taskintent::ParentRunRef>,
    ) -> Result<Self, SupervisorAdmissionError> {
        let profile =
            registry
                .resolve(vref)
                .ok_or_else(|| SupervisorAdmissionError::UnresolvedProfile {
                    profile_id: vref.profile_id.clone(),
                    revision: vref.revision,
                    digest: vref.digest.clone(),
                })?;
        if profile.role != SubAgentRoleV1::Supervisor {
            return Err(SupervisorAdmissionError::RoleNotSupervisor {
                profile_id: profile.profile_id,
            });
        }
        if spawning_lineage.depth() > 0 {
            return Err(SupervisorAdmissionError::DepthDenied {
                depth: spawning_lineage.depth(),
            });
        }
        let run_ref =
            RunRef::from_opaque(format!("supervisor-v1-{}", uuid::Uuid::new_v4().simple()));
        // The wire body is a minted uuid run id — bounded by construction
        // (documented invariant; the own() length cap cannot fire).
        let supervisor_wire_ref = zeroclaw_api::taskintent::SubAgentRunRef::own(run_ref.as_str())
            .expect("run-scoped supervisor wire ref is bounded by construction");
        Ok(Self {
            granted: profile.supervisor_authority_set.iter().copied().collect(),
            pinned_digest: profile.digest.clone(),
            profile,
            run_ref,
            supervisor_wire_ref,
            requester,
            parent_ref,
            client,
            phase: SupervisorPhase::Planning,
            planned: false,
            implementation_task: None,
            implementation_bundle_ref: None,
            implementation_intent_digest: None,
            reviews: Vec::new(),
            proposals: Vec::new(),
            meter: Arc::new(SubAgentBudgetMeter::new(
                zeroclaw_api::subagent_v1::SubAgentBudgetV1 {
                    // A supervision session is bridge-op bound, not model
                    // bound; the token ceiling records zero-model usage.
                    time_limit_secs: 600,
                    max_tokens: 0,
                    max_actions: 64,
                },
            )),
            bridge_ops: 0,
            next_request_seq: 0,
        })
    }

    /// The authority-minted, run-scoped identity (SA-13).
    #[must_use]
    pub fn run_ref(&self) -> &RunRef {
        &self.run_ref
    }

    /// The session's granted authority set (the typed SA-29 set — never
    /// one `can_manage_tachi` bit).
    #[must_use]
    pub fn granted_authorities(&self) -> &BTreeSet<SupervisorAuthority> {
        &self.granted
    }

    /// The current phase (typed state machine).
    #[must_use]
    pub fn phase(&self) -> SupervisorPhase {
        self.phase
    }

    /// Typed inventory of everything the session holds. The type itself
    /// is the negative-capability evidence: there is no field for a
    /// credential, a shell/process handle, a filesystem root, a tool
    /// registry, or a spawn surface, because the session cannot hold
    /// any of them (SA-29/SA-30; N-R2 boundary 2). The serialized key
    /// set is pinned by test — adding an execution-shaped field becomes
    /// observable.
    #[must_use]
    pub fn inventory(&self) -> SupervisorInventory {
        SupervisorInventory {
            profile_id: self.profile.profile_id.clone(),
            role: "supervisor".to_string(),
            granted_authorities: self
                .granted
                .iter()
                .map(|authority| format!("{authority:?}").to_lowercase())
                .collect(),
            supervisor_ref: self.run_ref.as_str().to_string(),
            phase: format!("{:?}", self.phase).to_lowercase(),
            bridge_operations: self.bridge_ops,
            budget_max_actions: self.meter.budget().max_actions,
            outbound_channel: "structured-report-only".to_string(),
        }
    }

    fn require(&self, authority: SupervisorAuthority) -> Result<(), SupervisorFlowError> {
        if self.granted.contains(&authority) {
            Ok(())
        } else {
            Err(SupervisorFlowError::AuthorityNotGranted { authority })
        }
    }

    /// SA-27: bridge operations are billable actions; the meter is
    /// minted per session and enforced before every op.
    fn record_action(&mut self, needed: &'static str) -> Result<(), SupervisorFlowError> {
        if self.meter.try_record_action() {
            self.bridge_ops += 1;
            Ok(())
        } else {
            Err(SupervisorFlowError::BudgetExhausted { needed })
        }
    }

    fn fresh_request_id(&mut self, prefix: &str) -> RequestId {
        self.next_request_seq += 1;
        RequestId::new(format!(
            "supv-{}-{prefix}-{}",
            self.run_ref.as_str(),
            self.next_request_seq
        ))
        .expect("run-scoped request id is bounded")
    }

    fn fresh_bundle_ref(&mut self, prefix: &str) -> String {
        format!("{}-bundle-{}", prefix, uuid::Uuid::new_v4().simple())
    }

    fn bounded(value: String) -> Result<BoundedText, SupervisorFlowError> {
        let len = value.len();
        BoundedText::new(value).map_err(|_| SupervisorFlowError::InternalBounds { len })
    }

    /// Compose the implementation Task A intent and hand it to the Parent
    /// as a TYPED action (SA-29's role-exclusive law: the SA-29 authority
    /// set has no submit operation, so the PARENT performs the initial
    /// `submit` on this request; the Tachi-minted TaskRef comes back via
    /// [`Self::attach_implementation_task`]). This is the production
    /// caller of the frozen five-value composer (`compose_intent`).
    ///
    /// The authority-bearing wire fields (`workspace_source`,
    /// `routing_preference`, `approval_requirement`, `privacy_class`)
    /// come from `parent_policy` — the PARENT's own admitted policy
    /// (TB-4 seam law); the session contributes content only.
    pub fn plan_implementation_task(
        &mut self,
        inputs: &TaskIntentInputs,
        parent_policy: &crate::tachi_bridge::RequesterBridgePolicy,
    ) -> Result<RequestedParentAction, SupervisorFlowError> {
        if self.phase != SupervisorPhase::Planning || self.planned {
            return Err(SupervisorFlowError::WrongPhase {
                phase: self.phase,
                reason: "one implementation intent per session; attach the submitted task next",
            });
        }
        let bundle_ref = self.fresh_bundle_ref("impl");
        let request_id = self.fresh_request_id("impl-submit");
        let context = StructuralIntentContext {
            requester: self.requester.clone(),
            parent_ref: self.parent_ref.clone(),
            supervisor_ref: Some(self.supervisor_wire_ref.clone()),
            context_bundle_ref: Self::bounded(bundle_ref.clone())?,
            source_refs: Vec::new(),
            expiry: None,
            retry_of: None,
        };
        let intent = compose_intent(inputs, parent_policy, &context)?;
        let digest = intent.canonical_digest();
        let subject = format!("intent:{}", &digest[..digest.len().min(16)]);
        self.implementation_bundle_ref = Some(bundle_ref);
        self.implementation_intent_digest = Some(digest);
        self.planned = true;
        Ok(RequestedParentAction {
            action: ParentActionKind::SubmitTaskIntent,
            subject_ref: subject,
            task_intent_request: Some(TaskIntentSubmitRequest { intent, request_id }),
        })
    }

    /// The Parent hands the Tachi-minted implementation TaskRef back to
    /// the session for supervision (the second half of the SA-29
    /// role-exclusive law).
    pub fn attach_implementation_task(
        &mut self,
        task_ref: TaskRef,
    ) -> Result<(), SupervisorFlowError> {
        if self.phase != SupervisorPhase::Planning || !self.planned {
            return Err(SupervisorFlowError::WrongPhase {
                phase: self.phase,
                reason: "the implementation task attaches after one planned intent",
            });
        }
        self.implementation_task = Some(task_ref);
        self.phase = SupervisorPhase::Supervising;
        Ok(())
    }

    fn implementation_task_ref(&self) -> Result<TaskRef, SupervisorFlowError> {
        self.implementation_task
            .clone()
            .ok_or(SupervisorFlowError::NoImplementationTask)
    }

    /// `ObserveTask` (SA-29): the task snapshot, per-dimension projected
    /// through the bridge's TB-16 tables.
    pub async fn observe(
        &mut self,
        task_ref: &TaskRef,
    ) -> Result<TaskSnapshotView, SupervisorFlowError> {
        self.require(SupervisorAuthority::ObserveTask)?;
        self.record_action("observe")?;
        self.client
            .get(task_ref)
            .await
            .map_err(|error| SupervisorFlowError::ObserveFailed {
                task_ref: task_ref.clone(),
                error,
            })
    }

    /// `ReadResultRefs` (SA-29): the artifact/evidence-first result
    /// projection (TB-13).
    pub async fn collect_result(
        &mut self,
        task_ref: &TaskRef,
    ) -> Result<ResultProjectionView, SupervisorFlowError> {
        self.require(SupervisorAuthority::ReadResultRefs)?;
        self.record_action("collect")?;
        self.client.collect_latest(task_ref).await.map_err(|error| {
            SupervisorFlowError::CollectFailed {
                task_ref: task_ref.clone(),
                error,
            }
        })
    }

    /// `RequestIndependentReview` (SA-29/TB-11/TB-17): maps to a NEW
    /// review task on a distinct Task/context lineage with its
    /// independence class recorded — the mapping the TB-11 law names
    /// ("independent review creates separate task/attempt/context
    /// lineage and must satisfy an explicit independence class"; the
    /// session-intervention path on both sides refuses the op with
    /// `requires_new_task_lineage`, which is exactly why this method
    /// SUBMITS a fresh task instead).
    ///
    /// Client-side discrimination gates, in order:
    /// 1. the authority must be granted;
    /// 2. the class must be independence-marked — a continuation or a
    ///    deterministic check can never satisfy an independent-review
    ///    requirement, so they are refused as the review's class HERE
    ///    (the pre-check half of the TB-17 joint test);
    /// 3. the review's context bundle ref is freshly minted and must
    ///    differ from the implementation task's (lineage distinctness).
    ///
    /// The submitted intent is structurally a REVIEW: the capability is
    /// forced to `reasoning_review` (the session's only submit surface
    /// can never mint an implementation task), the supervisor wire ref
    /// names this session, and the intent's evaluation requirement
    /// records the review's own independence class.
    pub async fn request_independent_review(
        &mut self,
        required_class: IndependenceClass,
        objective: &str,
    ) -> Result<ReviewLineageRecord, SupervisorFlowError> {
        self.require(SupervisorAuthority::RequestIndependentReview)?;
        if !required_class.is_independence_marked() {
            return Err(SupervisorFlowError::ClassNotIndependenceMarked {
                class: required_class,
            });
        }
        let review_of = self.implementation_task_ref()?;
        let implementation_bundle = self
            .implementation_bundle_ref
            .clone()
            .unwrap_or_else(|| "unknown-impl-bundle".to_string());
        let bundle_ref = self.fresh_bundle_ref("review");
        if bundle_ref == implementation_bundle {
            // Defence in depth: a mint collision would collapse lineage.
            return Err(SupervisorFlowError::FreshBundleMintFailed);
        }
        let request_id = self.fresh_request_id("review-submit");
        // The review references its subject by the implementation
        // intent's CANONICAL DIGEST, never by the `task:` wire value —
        // the encode-side admission law forbids task/attempt wire
        // values inside text-bearing fields (TB-4 CallerMintedRef
        // category), so the objective names the digest and the lineage
        // truth stays in the typed records.
        let subject_digest = self
            .implementation_intent_digest
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let objective = Self::bounded(format!(
            "Independent review (class {required_class:?}) of the artifact produced under              implementation intent digest {subject_digest}: {objective}"
        ))?;
        let inputs = TaskIntentInputs {
            objective,
            capability_request: CapabilityRequest {
                capability: Capability::ReasoningReview,
            },
            constraints: vec![TaskConstraint {
                description: Self::bounded(
                    "fresh-context review only; the review must not share the implementation \
                     task's context lineage; report findings as evidence-backed statements"
                        .to_string(),
                )?,
            }],
            expected_artifacts: Vec::new(),
            evaluation_requirement: zeroclaw_api::taskintent::EvaluationRequirement {
                independence: required_class,
            },
        };
        // The review policy is the SESSION's own: admitted capability is
        // ONLY reasoning_review — the least authority a review submission
        // can carry. Workspace/routing/approval inherit the parent's
        // posture by construction (None/defaults), never task input.
        let review_policy = crate::tachi_bridge::RequesterBridgePolicy {
            admitted_capabilities: BTreeSet::from([Capability::ReasoningReview]),
            workspace_source: None,
            routing_preference: None,
            approval_requirement: zeroclaw_api::taskintent::ApprovalRequirement::NotRequired,
            privacy_class: zeroclaw_api::taskintent::PrivacyClass::Internal,
        };
        let context = StructuralIntentContext {
            requester: self.requester.clone(),
            parent_ref: self.parent_ref.clone(),
            supervisor_ref: Some(self.supervisor_wire_ref.clone()),
            context_bundle_ref: Self::bounded(bundle_ref.clone())?,
            source_refs: Vec::new(),
            expiry: None,
            retry_of: None,
        };
        let intent = compose_intent(&inputs, &review_policy, &context)?;
        self.record_action("review submit")?;
        let receipt = self.client.submit(&intent, &request_id).await;
        match receipt {
            Ok(SubmitReceipt::Admitted {
                task_ref,
                replayed: _,
            }) => {
                let record = ReviewLineageRecord {
                    review_task: task_ref,
                    review_of,
                    independence_class: required_class,
                    context_bundle_ref: bundle_ref,
                    implementation_bundle_ref: implementation_bundle,
                    request_id: request_id.to_string(),
                };
                self.reviews.push(record.clone());
                Ok(record)
            }
            Ok(other) => Err(SupervisorFlowError::ReviewSubmitRefused { receipt: other }),
            // TB-7 rule 4: replay the SAME tuple — never a new id. The
            // reconciling submit does that internally; a hard transport
            // error surfaces as a refused submission (fail closed, no
            // local fallback).
            Err(_) => Err(SupervisorFlowError::ReviewSubmitRefused {
                receipt: SubmitReceipt::Unavailable,
            }),
        }
    }

    /// All review lineage records this session minted, in order.
    #[must_use]
    pub fn review_records(&self) -> &[ReviewLineageRecord] {
        &self.reviews
    }

    /// Whether the latest review satisfies the given independence-marked
    /// requirement (the acceptance gate the Parent consults).
    #[must_use]
    pub fn latest_review_satisfies(&self, required: IndependenceClass) -> bool {
        self.reviews
            .last()
            .is_some_and(|record| record.satisfies(required))
    }

    /// A session intervention on the implementation task, gated by the
    /// granted authority set (`RequestCorrection` / `RequestContinuation`
    /// / `ProvideContext` / `RequestUserInput` / the two stop requests —
    /// SA-29; `RequestPause`/`RequestResume`/`Escalate` are structurally
    /// unrepresentable in [`SupervisorIntervention`]).
    pub async fn intervene_on_implementation(
        &mut self,
        op: SupervisorIntervention,
    ) -> Result<InterventionReceipt, SupervisorFlowError> {
        let task_ref = self.implementation_task_ref()?;
        let request_id = self.fresh_request_id("intervene");
        self.record_action("intervene")?;
        self.client
            .supervisor_intervene(
                &self.granted,
                op,
                &task_ref,
                &self.requester,
                &request_id,
                None,
            )
            .await
            .map_err(SupervisorFlowError::Intervention)
    }

    /// `ProposeJudgment` (SA-29): a receipt-bound, run-scoped
    /// interpretation over Tachi's adjudication truth. The verdict field
    /// is the ADJUDICATION dimension of the collected projection and
    /// nothing else — the worker's terminal classification rides along
    /// as an observed fact, explicitly not the verdict (TB-13/TB-18).
    /// This method performs NO bridge write: a proposal is not a
    /// lifecycle transition, and canonical adjudication state stays
    /// Tachi's (KP-21).
    pub async fn propose_judgment(
        &mut self,
        task_ref: &TaskRef,
    ) -> Result<JudgmentProposalV1, SupervisorFlowError> {
        self.require(SupervisorAuthority::ProposeJudgment)?;
        let projection = self.collect_result(task_ref).await?;
        let proposal = JudgmentProposalV1 {
            proposal_id: format!(
                "judgment-{}-{}",
                self.run_ref.as_str(),
                uuid::Uuid::new_v4().simple()
            ),
            task_ref: task_ref.clone(),
            result_revision: projection.result_revision,
            adjudication_observed: projection.adjudication.label().to_string(),
            terminal_classification_observed: projection.terminal_classification.clone(),
            proposed_interpretation: format!(
                "interpretation only: task {} carries adjudication `{}` at result revision {}; \
                 the worker terminal classification `{}` is an observed fact, not the verdict",
                task_ref.as_wire(),
                projection.adjudication.label(),
                projection.result_revision,
                projection.terminal_classification
            ),
            evidence_refs: projection.artifact_evidence_refs.clone(),
        };
        self.proposals.push(proposal.clone());
        Ok(proposal)
    }

    /// The judgment proposals this session produced (run-scoped;
    /// interpretation only).
    #[must_use]
    pub fn judgment_proposals(&self) -> &[JudgmentProposalV1] {
        &self.proposals
    }

    /// Assemble the terminal SupervisorReport — a plain
    /// [`zeroclaw_api::subagent_v1::SubAgentReportV1`] (SA-21: the ONLY
    /// child→parent result channel; the Supervisor never messages the
    /// user and holds no channel handle — SA-1/SA-2/SA-25). The summary
    /// is interpretation; the verdict-shaped facts are the recorded
    /// adjudication labels, each traceable to a receipt.
    #[must_use]
    pub fn conclude(mut self) -> zeroclaw_api::subagent_v1::SubAgentReportV1 {
        use zeroclaw_api::subagent_v1::{Finding, SubAgentUsage};
        let mut findings = Vec::new();
        let mut evidence_refs = Vec::new();
        let mut recommendations = Vec::new();
        for (index, record) in self.reviews.iter().enumerate() {
            findings.push(Finding {
                finding_id: format!("review-{}", index + 1),
                statement: format!(
                    "independent review {} of {} (class {:?}, lineage {} vs {})",
                    record.review_task.as_wire(),
                    record.review_of.as_wire(),
                    record.independence_class,
                    record.context_bundle_ref,
                    record.implementation_bundle_ref,
                ),
                evidence_refs: Vec::new(),
            });
        }
        for (index, proposal) in self.proposals.iter().enumerate() {
            findings.push(Finding {
                finding_id: format!("judgment-{}", index + 1),
                statement: format!(
                    "proposed judgment for {}: adjudication `{}` (proposal — not canonical \
                     truth); worker classification `{}` recorded as fact",
                    proposal.task_ref.as_wire(),
                    proposal.adjudication_observed,
                    proposal.terminal_classification_observed,
                ),
                evidence_refs: proposal
                    .evidence_refs
                    .iter()
                    .map(|r| zeroclaw_api::subagent_v1::EvidenceRef(r.clone()))
                    .collect(),
            });
            evidence_refs.extend(proposal.evidence_refs.iter().cloned());
            recommendations.push(zeroclaw_api::subagent_v1::Recommendation {
                recommendation_id: format!("rec-{}", index + 1),
                statement: format!(
                    "present judgment proposal {} as interpretation; canonical adjudication \
                     remains Tachi-side",
                    proposal.proposal_id
                ),
                evidence_refs: Vec::new(),
            });
        }
        let usage = SubAgentUsage {
            elapsed_ms: self.meter.usage().elapsed_ms,
            tokens_in: 0,
            tokens_out: 0,
            actions: self.bridge_ops,
        };
        self.phase = SupervisorPhase::Concluded;
        zeroclaw_api::subagent_v1::SubAgentReportV1 {
            run_ref: self.run_ref.clone(),
            profile_ref: VersionedProfileRef {
                profile_id: self.profile.profile_id.clone(),
                revision: self.profile.revision,
                digest: self.pinned_digest.clone(),
            },
            context_bundle_ref: self
                .implementation_bundle_ref
                .clone()
                .unwrap_or_else(|| "none".to_string()),
            status: SubAgentTerminalFact::Completed,
            summary: format!(
                "supervision concluded: {} review lineage record(s), {} judgment proposal(s); \
                 all verdicts are Tachi-side adjudication facts, none minted locally",
                self.reviews.len(),
                self.proposals.len()
            ),
            findings,
            evidence_refs: evidence_refs
                .into_iter()
                .map(zeroclaw_api::subagent_v1::EvidenceRef)
                .collect(),
            uncertainty: Vec::new(),
            recommendations,
            requested_parent_actions: Vec::new(),
            proposed_candidates: Vec::new(),
            usage,
        }
    }
}

/// What a supervisor session contains (the negative-capability
/// inventory). No credential, shell, filesystem, tool-registry, or
/// spawn-surface fields exist on this type — the serialized key set is
/// the inventory, pinned by test so an execution-shaped addition becomes
/// observable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SupervisorInventory {
    pub profile_id: String,
    pub role: String,
    pub granted_authorities: Vec<String>,
    pub supervisor_ref: String,
    pub phase: String,
    pub bridge_operations: u32,
    pub budget_max_actions: u32,
    pub outbound_channel: String,
}
