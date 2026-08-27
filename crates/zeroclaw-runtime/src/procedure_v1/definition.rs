//! Definition capture: read an SOP package ONCE and project it into a
//! `ProcedureDefinitionV1` from exactly those bytes (KP-11 publication
//! rules 2–3: complete-definition atomicity, race-free mint).
//!
//! The capture is a single `read` per file; the manifest, the parsed
//! steps, and the digest all derive from the CAPTURED bytes, never from
//! a second read — there is no window where parsed steps and digested
//! bytes can disagree (no TOCTOU on the content hash).
//!
//! Publication state (KP-11 rule 1): the authored `[sop] review_state`
//! key (`"draft"` | `"published"`); ABSENT means draft — fail closed.
//! The legacy `SopManifest` carries no `deny_unknown_fields`, so the
//! key is backward-compatible: the legacy loader ignores it. Review
//! state is definition-side authored state (KP-10 — authoring, revision
//! creation, applicability, and review state stay ZeroClaw-owned);
//! flipping it to draft later does not retro-invalidate an already
//! minted snapshot, because the snapshot binds the published revision's
//! digest, immutably.

use std::path::Path;

use anyhow::{Context, Result, bail};
use zeroclaw_api::procedure_v1::{
    ArtifactExpectationV1, DefinitionProvenance, DefinitionReviewState, EvaluationContractRef,
    ProcedureDefinitionV1, ProcedureGateV1, ProcedureStepV1,
};
use zeroclaw_api::taskintent::{ArtifactClass, PrivacyClass, canonical_json_digest_hex};
use zeroclaw_log::{Action, Event, EventOutcome};

use crate::sop::parse_steps;
use crate::sop::types::{SopManifest, SopStep, SopStepKind};

/// One complete captured package revision — the single-read snapshot of
/// the authored bytes plus everything derived from them.
#[derive(Debug, Clone)]
pub struct CapturedDefinition {
    /// The exact captured `SOP.toml` bytes.
    pub toml_bytes: String,
    /// The exact captured `SOP.md` bytes (empty when the package has no
    /// markdown file).
    pub md_bytes: String,
    /// The parsed manifest (from the captured TOML bytes only).
    pub manifest: SopManifest,
    /// The authored review state read from the raw manifest table.
    pub review_state: DefinitionReviewState,
    /// The parsed steps (from the captured bytes only).
    pub steps: Vec<SopStep>,
    /// Canonical digest over the captured bytes.
    pub digest: String,
    /// Where the capture happened (definitions root, for provenance).
    pub sops_dir: std::path::PathBuf,
    /// The package name (directory key == procedure identity).
    pub name: String,
}

/// Read the review state from the RAW manifest table — the typed
/// `SopMeta` does not carry it (legacy-compatible extra key). Absent or
/// unrecognized values are DRAFT: publication must be an explicit
/// authored act (fail closed, KP-11 rule 1).
fn review_state_from_raw(toml_bytes: &str) -> Result<DefinitionReviewState> {
    let value: toml::Value = toml::from_str(toml_bytes).context("SOP.toml is not valid TOML")?;
    let Some(sop_table) = value.get("sop") else {
        bail!("SOP.toml has no [sop] table");
    };
    match sop_table.get("review_state").and_then(|v| v.as_str()) {
        Some("published") => Ok(DefinitionReviewState::Published),
        Some("draft") | None => Ok(DefinitionReviewState::Draft),
        Some(other) => {
            // Unknown labels refuse the capture entirely: an author who
            // typos the publication key gets a loud failure, not a
            // silent draft.
            bail!("unknown review_state value `{other}` (expected draft|published)")
        }
    }
}

/// Renumber manifest-carried steps into the dense 1..=N invariant the
/// authoring side enforces on save (the loader-side normalizer is
/// private to the legacy module; the invariant itself is a documented
/// derived property of the format).
fn renumber(steps: Vec<SopStep>) -> Vec<SopStep> {
    let mut steps = steps;
    for (index, step) in steps.iter_mut().enumerate() {
        step.number = (index as u32) + 1;
    }
    steps
}

/// Capture one SOP package by name under `sops_dir`. Reads
/// `SOP.toml` (required) and `SOP.md` (optional) exactly once each and
/// derives everything from the captured bytes. Rejects path-escape
/// names the same way the legacy loader does (single normal path
/// component).
pub fn capture_definition(sops_dir: &Path, name: &str) -> Result<CapturedDefinition> {
    let mut components = Path::new(name).components();
    let valid = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && !name.is_empty()
        && !name.starts_with('.');
    if !valid {
        bail!("invalid SOP name `{name}`");
    }
    let sop_dir = sops_dir.join(name);
    let toml_path = sop_dir.join("SOP.toml");
    let md_path = sop_dir.join("SOP.md");

    // Single-read capture per file (KP-11 rules 2–3): everything below
    // derives from these bytes; nothing re-reads the tree.
    let toml_bytes = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("reading {}", toml_path.display()))?;
    let md_bytes = if md_path.exists() {
        std::fs::read_to_string(&md_path)
            .with_context(|| format!("reading {}", md_path.display()))?
    } else {
        String::new()
    };

    let manifest: SopManifest =
        toml::from_str(&toml_bytes).context("SOP.toml manifest decode failed")?;
    let review_state = review_state_from_raw(&toml_bytes)?;
    let steps = if !md_bytes.is_empty() {
        parse_steps(&md_bytes)
    } else {
        renumber(manifest.steps.clone())
    };
    if steps.is_empty() {
        bail!("SOP `{name}` has no steps (missing or empty SOP.md)");
    }

    let digest = canonical_json_digest_hex(&serde_json::json!({
        "sop_toml": toml_bytes,
        "sop_md": md_bytes,
    }));

    Ok(CapturedDefinition {
        toml_bytes,
        md_bytes,
        manifest,
        review_state,
        steps,
        digest,
        sops_dir: sops_dir.to_path_buf(),
        name: name.to_string(),
    })
}

/// Project a captured revision into the wire `ProcedureDefinitionV1`
/// (KP-10 field freeze). Pure function over the capture — no I/O.
pub fn project_definition(captured: &CapturedDefinition) -> ProcedureDefinitionV1 {
    let manifest = &captured.manifest;
    let applicability = manifest
        .triggers
        .iter()
        .map(|trigger| {
            // The serde token of the trigger variant (e.g. `manual`,
            // `mqtt`) — a stable machine summary, not free prose.
            let value = serde_json::to_value(trigger).unwrap_or(serde_json::Value::Null);
            match value {
                serde_json::Value::Object(map) => map
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string()),
                _ => "unknown".to_string(),
            }
        })
        .collect();

    let steps = captured
        .steps
        .iter()
        .map(|step| ProcedureStepV1 {
            number: step.number,
            title: step.title.clone(),
            body: zeroclaw_api::taskintent::BoundedText::new(step.body.clone())
                .unwrap_or_else(|_| {
                    // BoundedText's own limit is the wire bound; a body
                    // past it is clamped OUT of the projection by
                    // refusing the step — but a hard failure here would
                    // panic in a projection fn, so clamp to empty and
                    // let the mint's own bound checks refuse oversize
                    // snapshots with the typed error.
                    ::zeroclaw_log::record!(
                        WARN,
                        Event::new(module_path!(), Action::Reject)
                            .with_outcome(EventOutcome::Failure)
                            .with_attrs(serde_json::json!({
                                "sop": captured.name, "step": step.number,
                            })),
                        "procedure_v1: step body exceeds the bounded-text wire limit; projected empty (mint bound checks will refuse)"
                    );
                    zeroclaw_api::taskintent::BoundedText::new(String::new())
                        .expect("empty bounded text")
                }),
            suggested_tools: step.suggested_tools.clone(),
            requires_confirmation: step.requires_confirmation,
            kind: match step.kind {
                SopStepKind::Execute => "execute".to_string(),
                SopStepKind::Checkpoint => "checkpoint".to_string(),
                SopStepKind::Capability => "capability".to_string(),
            },
        })
        .collect();

    let approval_gates = captured
        .steps
        .iter()
        .filter(|step| step.requires_confirmation || step.kind == SopStepKind::Checkpoint)
        .map(|step| ProcedureGateV1 {
            step: step.number,
            policy: None,
        })
        .collect();

    // Deterministic, documented artifact expectations: a procedure run
    // always owes a final outcome report, and per-step output contracts
    // (when any step declares one) owe a verification trail.
    let mut expected_artifacts = vec![ArtifactExpectationV1 {
        artifact_class: ArtifactClass::Report,
        description: zeroclaw_api::taskintent::BoundedText::new(
            "Final procedure-run outcome report recorded through the bridge",
        )
        .expect("static bounded text"),
        required: true,
    }];
    if captured.steps.iter().any(|step| step.schema.is_some()) {
        expected_artifacts.push(ArtifactExpectationV1 {
            artifact_class: ArtifactClass::VerificationLog,
            description: zeroclaw_api::taskintent::BoundedText::new(
                "Per-step output-contract verification trail",
            )
            .expect("static bounded text"),
            required: true,
        });
    }

    ProcedureDefinitionV1 {
        procedure_id: captured.name.clone(),
        revision: manifest.sop.version.clone(),
        digest: captured.digest.clone(),
        name: manifest.sop.name.clone(),
        purpose: zeroclaw_api::taskintent::BoundedText::new(manifest.sop.description.clone())
            .unwrap_or_else(|_| {
                zeroclaw_api::taskintent::BoundedText::new(String::new())
                    .expect("empty bounded text")
            }),
        applicability,
        steps,
        approval_gates,
        constraints: Vec::new(),
        expected_artifacts,
        evaluation_contract: EvaluationContractRef {
            revision: "procedure-eval.v1".to_string(),
            digest: captured.digest.clone(),
        },
        privacy_class: PrivacyClass::Public,
        provenance: DefinitionProvenance {
            authored_via: "zeroclaw-sops-dir".to_string(),
            captured_at: chrono_now_rfc3339(),
        },
        review_state: captured.review_state,
    }
}

fn chrono_now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
