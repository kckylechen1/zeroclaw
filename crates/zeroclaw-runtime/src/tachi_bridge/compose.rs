//! Parent-side intent composition and encode-side admission (vertical
//! V2b; contract law TB-3/TB-4/TB-5).
//!
//! The task-specific expression surface is EXACTLY five values
//! ([`TaskIntentInputs`]): objective, capability_request, constraints,
//! expected_artifacts, evaluation_requirement. Every other wire field is
//! filled structurally ([`StructuralIntentContext`]) or from the
//! requester's own admitted policy ([`RequesterBridgePolicy`]) — the
//! authority-bearing subset (`capability_request`, `workspace_source`,
//! `routing_preference`, `approval_requirement`) never originates from
//! bundle/guidance content (TB-4 seam law; RULING-205 §1).
//!
//! Encode-side admission ([`scan_intent`]) mirrors the tachi host's
//! forbidden-content categories so a violation is caught BEFORE anything
//! is sent; the host-side admission remains authoritative. The mirrored
//! lists are byte-identical to the host's; on top of them the client
//! runs a STRICT SUPERSET watershed layer (`ExecutionDetail`): vendor /
//! model / worktree / cwd / tmux-SSH / sandbox / CLI-flag dimensions are
//! rejected as PROSE anywhere in a text-bearing value. The client may
//! reject more than the host; it may never reject less.

use std::collections::BTreeSet;

use zeroclaw_api::taskintent::{
    ApprovalRequirement, ArtifactExpectation, AttemptRef, BoundedText, Capability,
    CapabilityRequest, EvaluationRequirement, ParentRunRef, PrivacyClass, RoutingPreference,
    SCHEMA_TAG, SourceRef, SubAgentRunRef, TaskConstraint, TaskIntentV1, TaskRef, Timestamp,
    WorkspaceSourceRef,
};

// ─────────────────────────────────────────────────────────────────────────
// The five-value task-specific surface (DoD row 2)
// ─────────────────────────────────────────────────────────────────────────

/// The task-specific expression surface — EXACTLY the five owner-listed
/// values of the watershed ticket: objective, capability_request, constraints,
/// expected_artifacts, evaluation_requirement. Nothing else on the wire
/// originates from task-specific input.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskIntentInputs {
    /// What the requester wants accomplished (bounded, content-scanned).
    pub objective: BoundedText,
    /// The capability being requested (closed enum; this leaf's
    /// acceptance capability is `RepositoryImplementation`).
    pub capability_request: CapabilityRequest,
    /// Semantic constraints on the work.
    pub constraints: Vec<TaskConstraint>,
    /// What artifacts satisfy the request (drives TB-13 collect).
    pub expected_artifacts: Vec<ArtifactExpectation>,
    /// Evaluation independence requirement.
    pub evaluation_requirement: EvaluationRequirement,
}

/// The requester's own admitted policy — the ONLY source of the
/// authority-bearing wire fields (TB-4 seam law: bundle/guidance content
/// can never set these). Deliberately no field here is derivable from
/// task input or guidance text.
#[derive(Debug, Clone, PartialEq)]
pub struct RequesterBridgePolicy {
    /// Capabilities the requester's own profile/policy already permits
    /// (TB-5 intersection law: a request outside this set is refused at
    /// compose time, before any transport is touched).
    pub admitted_capabilities: BTreeSet<Capability>,
    /// Default workspace selector (typed repo/revision; never a path).
    pub workspace_source: Option<WorkspaceSourceRef>,
    /// Default routing preference (preference only — grants nothing).
    pub routing_preference: Option<RoutingPreference>,
    /// Default approval requirement assertion.
    pub approval_requirement: ApprovalRequirement,
    /// Default privacy class of intent content.
    pub privacy_class: PrivacyClass,
}

/// Structural (non-authority) context for one submission: identity and
/// lineage the runtime already holds, filled mechanically.
#[derive(Debug, Clone, PartialEq)]
pub struct StructuralIntentContext {
    /// The submitting requester (a claim; the host verifies it).
    pub requester: zeroclaw_api::taskintent::RequesterRef,
    /// Parent run lineage, if the submission belongs to one.
    pub parent_ref: Option<zeroclaw_api::taskintent::ParentRunRef>,
    /// Supervising sub-agent run, if any (V1 spine; None in this leaf's
    /// acceptance path).
    pub supervisor_ref: Option<zeroclaw_api::taskintent::SubAgentRunRef>,
    /// Opaque context-bundle reference (content, never authority).
    pub context_bundle_ref: BoundedText,
    /// Source material references.
    pub source_refs: Vec<SourceRef>,
    /// Optional expiry.
    pub expiry: Option<Timestamp>,
    /// Explicit retry lineage (TB-18): None for a first submission.
    pub retry_of: Option<TaskRef>,
}

/// Typed compose-time rejection (encode-side, fail closed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComposeRejection {
    /// A text-bearing value matched a forbidden-content category (TB-4).
    #[error("intent rejected: {category} in field `{field}`")]
    ForbiddenContent {
        /// The matched category.
        category: ForbiddenCategory,
        /// The wire field carrying the offending value.
        field: &'static str,
    },
    /// The requested capability is outside the requester's own admitted
    /// set (TB-5 intersection law; checked before any transport call).
    #[error("intent rejected: capability not admitted for requester")]
    CapabilityNotAdmitted,
}

/// TB-4 forbidden-content categories (mirrors the tachi host's
/// `ForbiddenCategory` one-for-one so client pre-flight and host
/// admission cannot disagree on vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ForbiddenCategory {
    /// Raw credential-shaped value (API keys, tokens, private keys).
    #[error("credential-shaped value")]
    Credential,
    /// Shell/SSH/tmux/container command text.
    #[error("cli/shell command text")]
    Command,
    /// Worktree/filesystem path used as execution authority.
    #[error("worktree-shaped path")]
    WorktreePath,
    /// Private-Dyad-labeled value.
    #[error("private-dyad-labeled value")]
    PrivateDyad,
    /// Caller-minted task/attempt id smuggled as content.
    #[error("caller-minted task/attempt id")]
    CallerMintedRef,
    /// Execution detail named as PROSE in a text-bearing value — a
    /// vendor/model name, worktree, cwd, tmux/SSH, sandbox, or CLI-flag
    /// token anywhere in the text (vertical V2b discrimination list).
    /// Client-side strict superset of the mirrored host categories: the
    /// host law stays authoritative host-side; this layer exists so the
    /// watershed dimensions are rejected before transport even when they
    /// are not shaped like commands or paths.
    #[error("execution detail named in text")]
    ExecutionDetail,
}

/// Compose failure that is not a policy/content rejection.
#[derive(Debug, thiserror::Error)]
#[error("compose failed: {0}")]
pub struct ComposeError(#[from] ComposeRejection);

/// Compose a `TaskIntentV1` from the five task-specific values, the
/// requester's admitted policy, and structural context.
///
/// Field provenance is fixed by construction:
/// - the five [`TaskIntentInputs`] values land verbatim in the intent;
/// - `capability_request` is checked against the policy's admitted set
///   (TB-5) and `workspace_source`/`routing_preference`/
///   `approval_requirement`/`privacy_class` come from the policy;
/// - everything else comes from [`StructuralIntentContext`].
///
/// Encode-side admission runs last; a rejection never produces an intent.
pub fn compose_intent(
    inputs: &TaskIntentInputs,
    policy: &RequesterBridgePolicy,
    context: &StructuralIntentContext,
) -> Result<TaskIntentV1, ComposeRejection> {
    if !policy
        .admitted_capabilities
        .contains(&inputs.capability_request.capability)
    {
        return Err(ComposeRejection::CapabilityNotAdmitted);
    }
    let intent = TaskIntentV1 {
        schema: SCHEMA_TAG.to_string(),
        objective: inputs.objective.clone(),
        capability_request: inputs.capability_request,
        requester: context.requester.clone(),
        parent_ref: context.parent_ref.clone(),
        supervisor_ref: context.supervisor_ref.clone(),
        context_bundle_ref: context.context_bundle_ref.clone(),
        source_refs: context.source_refs.clone(),
        constraints: inputs.constraints.clone(),
        expected_artifacts: inputs.expected_artifacts.clone(),
        evaluation_requirement: inputs.evaluation_requirement.clone(),
        workspace_source: policy.workspace_source.clone(),
        routing_preference: policy.routing_preference,
        approval_requirement: policy.approval_requirement,
        privacy_class: policy.privacy_class,
        expiry: context.expiry.clone(),
        retry_of: context.retry_of.clone(),
    };
    scan_intent(&intent)?;
    Ok(intent)
}

// ─────────────────────────────────────────────────────────────────────────
// Encode-side admission scan (TB-4 mirror of the tachi host law)
// ─────────────────────────────────────────────────────────────────────────

/// Substrings whose presence in any text-bearing value is
/// credential-shaped (TB-4 category 1). Byte-identical list to the tachi
/// host's `CREDENTIAL_MARKERS`.
const CREDENTIAL_MARKERS: &[&str] = &[
    "-----BEGIN OPENSSH PRIVATE KEY",
    "-----BEGIN RSA PRIVATE KEY",
    "-----BEGIN PRIVATE KEY",
    "-----BEGIN EC PRIVATE KEY",
    "sk-ant-",
    "sk-proj-",
    "ghp_",
    "github_pat_",
    "gho_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "api_key=",
    "apikey:",
    "password=",
    "bearer ",
];

/// Leading tokens that make a value a shell/SSH/tmux/container command
/// (TB-4 category 2; includes the harness CLI names the Parent must
/// never place in an intent). Byte-identical list to the tachi host's
/// `COMMAND_LEAD_TOKENS`.
const COMMAND_LEAD_TOKENS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "exec", "eval", "source", "sudo", "su", "ssh", "scp",
    "sftp", "mosh", "telnet", "tmux", "screen", "docker", "podman", "kubectl", "nerdctl", "git",
    "cargo", "npm", "pnpm", "yarn", "python", "python3", "node", "ruby", "codex", "claude",
    "gemini", "opencode", "aider", "grok", "rm", "mv", "cp", "chmod", "chown", "curl", "wget",
    "nc",
];

/// Markers that make a value a worktree/filesystem path (TB-4 category
/// 3). Byte-identical list to the tachi host's `WORKTREE_MARKERS`.
const WORKTREE_MARKERS: &[&str] = &[
    "/worktrees/",
    "worktree_path",
    ".git/",
    "/Users/",
    "/home/",
    "/tmp/",
    "/var/folders/",
    "\\.git\\",
];

/// Markers for Private-Dyad-labeled content (TB-4 category 4).
const PRIVATE_DYAD_MARKERS: &[&str] = &["private dyad", "private_dyad", "private-dyad"];

/// Vendor/model names the Parent must never place in any text-bearing
/// value (vertical V2b discrimination list: glm/codex/any model or
/// vendor name, TB-5). Matched on word boundaries over the lowercased
/// text so `Use Anthropic Claude as the backend` is rejected even though
/// it is not shaped like a command.
const WATERSHED_VENDOR_TOKENS: &[&str] = &[
    "glm",
    "codex",
    "claude",
    "anthropic",
    "openai",
    "chatgpt",
    "gpt",
    "gemini",
    "deepseek",
    "qwen",
    "kimi",
    "moonshot",
    "llama",
    "mistral",
    "grok",
    "aider",
    "opencode",
    "copilot",
    "cursor",
];

/// Execution-placement tokens banned ANYWHERE in a text-bearing value
/// (vertical V2b discrimination list: worktree, tmux/SSH, sandbox flags,
/// cwd — TB-4/TB-1). Word-boundary matched; `working directory` is
/// phrase-matched because it is two words.
const WATERSHED_PLACEMENT_TOKENS: &[&str] = &["worktree", "tmux", "ssh", "sandbox", "cwd"];
const WATERSHED_PLACEMENT_PHRASES: &[&str] = &["working directory"];

/// Scan one text-bearing value against every forbidden category. Returns
/// the first match by category order (same order as the host).
pub fn scan_text(field: &'static str, value: &BoundedText) -> Result<(), ComposeRejection> {
    scan_str(field, value.as_str())
}

/// Category scan over a raw string (shared engine for [`scan_text`] and
/// the ref-wire hardening below).
fn scan_str(field: &'static str, text: &str) -> Result<(), ComposeRejection> {
    let lower = text.to_ascii_lowercase();

    for marker in CREDENTIAL_MARKERS {
        if text.contains(marker) || lower.contains(&marker.to_ascii_lowercase()) {
            return Err(forbid(ForbiddenCategory::Credential, field));
        }
    }
    let first_token = lower.split_whitespace().next().unwrap_or("");
    if COMMAND_LEAD_TOKENS.contains(&first_token) {
        return Err(forbid(ForbiddenCategory::Command, field));
    }
    if lower.starts_with("./") || lower.starts_with('/') || lower.starts_with('~') {
        return Err(forbid(ForbiddenCategory::WorktreePath, field));
    }
    for marker in WORKTREE_MARKERS {
        if lower.contains(&marker.to_ascii_lowercase()) {
            return Err(forbid(ForbiddenCategory::WorktreePath, field));
        }
    }
    for marker in PRIVATE_DYAD_MARKERS {
        if lower.contains(marker) {
            return Err(forbid(ForbiddenCategory::PrivateDyad, field));
        }
    }
    if text.contains(TaskRef::WIRE_PREFIX) || text.contains(AttemptRef::WIRE_PREFIX) {
        return Err(forbid(ForbiddenCategory::CallerMintedRef, field));
    }

    // Client-side watershed layer (vertical V2b discrimination list).
    // This is a deliberate STRICT SUPERSET of the mirrored host law:
    // the host's five categories stay byte-identical above; this layer
    // exists because the watershed dimensions are semantic, not
    // shape-based — `name the model`, `use a worktree`, `pass --flag`
    // are forbidden as PROSE, wherever they appear. The client may
    // reject more than the host; it may never reject less.
    for word in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if WATERSHED_VENDOR_TOKENS.contains(&word) {
            return Err(forbid(ForbiddenCategory::ExecutionDetail, field));
        }
        if WATERSHED_PLACEMENT_TOKENS.contains(&word) {
            return Err(forbid(ForbiddenCategory::ExecutionDetail, field));
        }
    }
    for phrase in WATERSHED_PLACEMENT_PHRASES {
        if lower.contains(phrase) {
            return Err(forbid(ForbiddenCategory::ExecutionDetail, field));
        }
    }
    // Mid-string relative paths (`worktree ../feature-v2b`, `see
    // ./docs/x`) — the prefix checks above only catch leading paths.
    if lower.contains("../") || lower.contains(" ./") || lower.contains(" ~/") {
        return Err(forbid(ForbiddenCategory::ExecutionDetail, field));
    }
    // CLI flags as standalone tokens (`--fast`, `-rf`): whitespace-
    // delimited tokens that begin with `-` followed by a letter/digit.
    for token in lower.split_whitespace() {
        let stripped = token.trim_start_matches('-');
        if stripped.len() != token.len()
            && stripped.chars().next().is_some_and(char::is_alphanumeric)
        {
            return Err(forbid(ForbiddenCategory::ExecutionDetail, field));
        }
    }
    Ok(())
}

/// Scan EVERY text-bearing value of an intent (TB-4: "over every
/// text-bearing value").
pub fn scan_intent(intent: &TaskIntentV1) -> Result<(), ComposeRejection> {
    scan_text("objective", &intent.objective)?;
    scan_text("context_bundle_ref", &intent.context_bundle_ref)?;
    for source in &intent.source_refs {
        scan_text("source_refs.locator", &source.locator)?;
    }
    for constraint in &intent.constraints {
        scan_text("constraints.description", &constraint.description)?;
    }
    for artifact in &intent.expected_artifacts {
        scan_text("expected_artifacts.description", &artifact.description)?;
    }
    if let Some(workspace) = &intent.workspace_source {
        scan_text("workspace_source.repo", &workspace.repo)?;
        if let Some(git_ref) = &workspace.git_ref {
            scan_text("workspace_source.git_ref", git_ref)?;
        }
    }
    Ok(())
}

fn forbid(category: ForbiddenCategory, field: &'static str) -> ComposeRejection {
    ComposeRejection::ForbiddenContent { category, field }
}

/// ZeroClaw-side hardening BEYOND the mirrored host law: scan the wire
/// values of every ref the requester authors or carries — `requester`,
/// `parent_ref`, `supervisor_ref`, and `retry_of` — against the same five
/// forbidden categories, applied to the ref BODY (the wire value minus the
/// ref's own namespace prefix, so a legitimate `task:`-namespaced
/// `retry_of` body is not itself a caller-minted-ref hit).
///
/// The tachi host admission law scans the intent's `BoundedText` fields
/// only (that law is mirrored byte-for-byte by [`scan_intent`] and stays
/// authoritative host-side); this function is the CLIENT's fail-closed
/// layer over the fields ZeroClaw itself authors — a lineage ref or
/// requester claim carrying credential/command/worktree/private-dyad
/// content, or a caller-minted `task:`/`attempt:` body inside a lineage
/// ref, never reaches a transport.
pub fn scan_client_authored_refs(intent: &TaskIntentV1) -> Result<(), ComposeRejection> {
    scan_str("requester", &intent.requester.to_string())?;
    if let Some(parent) = &intent.parent_ref {
        scan_str(
            "parent_ref",
            parent
                .as_wire()
                .strip_prefix(ParentRunRef::WIRE_PREFIX)
                .unwrap_or(parent.as_wire()),
        )?;
    }
    if let Some(supervisor) = &intent.supervisor_ref {
        scan_str(
            "supervisor_ref",
            supervisor
                .as_wire()
                .strip_prefix(SubAgentRunRef::WIRE_PREFIX)
                .unwrap_or(supervisor.as_wire()),
        )?;
    }
    if let Some(prior) = &intent.retry_of {
        // A decoded prior TaskRef legitimately carries the `task:`
        // namespace — strip it and scan the body only.
        scan_str(
            "retry_of",
            prior
                .as_wire()
                .strip_prefix(TaskRef::WIRE_PREFIX)
                .unwrap_or(prior.as_wire()),
        )?;
    }
    Ok(())
}
