//! Vertical V4 discriminations (ticket #236 frozen checks; owner tests
//! 1/3/7/8/9 mapping). Every test runs against the in-memory bridge
//! double — the TACHI-side stand-in — proving the ZeroClaw half's laws:
//! snapshot immutability, run binding, no local run store, publication
//! rules, privacy, and the narrow-only capability seam.

use std::collections::BTreeSet;
use std::sync::Arc;

use zeroclaw_api::procedure_v1::{DefinitionReviewState, PROCEDURE_SNAPSHOT_REF_PREFIX};
use zeroclaw_api::subagent_v1::ProposedCandidateKind;
use zeroclaw_api::taskintent::{
    ApprovalRequirement, Capability, PrivacyClass, RequestId, RequesterRef, RoutingPreference,
    SCHEMA_TAG,
};

use super::definition::capture_definition;
use super::run::{ProcedureRunClient, ProcedureSubmitError, derive_request_id};
use super::snapshot::{SnapshotContentCategory, SnapshotMintError, mint_snapshot};
use crate::tachi_bridge::compose::RequesterBridgePolicy;
use crate::tachi_bridge::in_memory::InMemoryTachiTaskBridge;
use crate::tachi_bridge::procedure::ProcedureSubmitPort;
use crate::tachi_bridge::SubmitReceipt;

/// The StageX-derived conformance fixture: the documented reference SOP
/// (`docs/book/src/sop/example.md` — the in-tree "stagex-update"
/// reference deployment) materialized as an SOP package. This fixture
/// is the E2E definitions dir seed too; the steps are the documented
/// eight, verbatim in title/body/tools.
pub(crate) const STAGEX_TOML: &str = r#"[sop]
name = "stagex-update"
description = "Watch the upstream release feed, bump the StageX package, build it, verify it reproduces by digest, push the change, open a draft pull request, and announce the result."
version = "1.0.0"
deterministic = true
review_state = "published"

[[triggers]]
type = "manual"
"#;

pub(crate) const STAGEX_MD: &str = r#"## Steps

1. **Resolve** — Map the upstream project to the real StageX package; read the current version; stop if not strictly newer.
   - tools: shell, file_read

2. **Bump + hash** — Set the new version, run `make fetch`, write the correct source hash, re-fetch until clean.
   - tools: shell, file_write

3. **Build** — Build just this package; retry once on a hash failure.
   - tools: shell

4. **Patch if broken** — On a build break, refresh or source a patch using the local model; flag genuine API breaks.
   - tools: shell, file_read, file_write, http_request

5. **Digest repro** — Run `make digests`, build a second time, confirm the digest is unchanged.
   - tools: shell

6. **Commit + push** — Commit on a per-package, per-version branch; push to the fork.
   - tools: shell, git_operations

7. **Open draft PR** — Fill the PR template, attach digests, mark ready only on a clean reproduced build.
   - tools: http_request

8. **Announce** — Post the outcome to the announcement room: package, version delta, repro status, digest, PR link.
   - tools: shell
"#;

fn write_package(dir: &std::path::Path, toml_body: &str, md_body: &str) {
    let package = dir.join("stagex-update");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("SOP.toml"), toml_body).unwrap();
    std::fs::write(package.join("SOP.md"), md_body).unwrap();
}

fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path(), STAGEX_TOML, STAGEX_MD);
    dir
}

fn full_policy() -> RequesterBridgePolicy {
    RequesterBridgePolicy {
        admitted_capabilities: BTreeSet::from([
            Capability::ReasoningReview,
            Capability::ReadOnlyInvestigation,
            Capability::RepositoryImplementation,
        ]),
        workspace_source: None,
        routing_preference: Some(RoutingPreference::PreferTachiManaged),
        approval_requirement: ApprovalRequirement::NotRequired,
        privacy_class: PrivacyClass::Public,
    }
}

fn client_and_double() -> (ProcedureRunClient, Arc<InMemoryTachiTaskBridge>) {
    let double = Arc::new(InMemoryTachiTaskBridge::new());
    let client = ProcedureRunClient::new(double.clone(), double.clone());
    (client, double)
}

fn requester() -> RequesterRef {
    RequesterRef::try_from("requester:zeroclaw-procedure-v4".to_string()).expect("bounded")
}

// ── Owner test 1 / DoD row 5: live definitions cannot mutate live runs ───

#[tokio::test]
async fn mid_run_definition_mutation_leaves_run_on_pinned_snapshot() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    let reference = snapshot.snapshot_ref();
    let titles_before: Vec<String> = snapshot.steps.iter().map(|s| s.title.clone()).collect();

    let (client, double) = client_and_double();
    let request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "rel-bzip2-1.0.9",
    );
    let output = client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();
    assert!(!output.replayed);

    // MID-RUN: edit the definitions tree — rewrite every step's title
    // and bump the manifest version (the exact legacy defect: the old
    // engine resolves remaining steps live).
    let mutated_toml = STAGEX_TOML.replace("version = \"1.0.0\"", "version = \"2.0.0\"");
    let mutated_md = STAGEX_MD.replace("**Resolve**", "**ResolveMutated**");
    write_package(dir.path(), &mutated_toml, &mutated_md);

    // The Tachi side continues the run from the RETAINED snapshot.
    let (completed, gate) = double.drive_procedure_steps(&output.task_ref, &reference);
    assert_eq!(
        completed.len(),
        8,
        "stagex fixture has no gates: all steps run"
    );
    assert!(gate.is_none());
    let executed = double.retained_step_titles(&reference);
    assert_eq!(
        executed, titles_before,
        "executing truth is the retained bytes"
    );
    assert!(!executed.contains(&"ResolveMutated".to_string()));
    // The same truth on the durable fact log (the audit trail, not the
    // CAS): executed steps recorded from the retained bytes.
    let fact_titles: Vec<String> = double
        .procedure_executed_steps(&output.task_ref)
        .into_iter()
        .map(|(_, title, _)| title)
        .collect();
    assert_eq!(fact_titles, titles_before);

    // The mutated tree is a DIFFERENT revision: re-capture re-derives a
    // new digest and would mint a DIFFERENT snapshot — the pinned run is
    // untouched.
    let recaptured = capture_definition(dir.path(), "stagex-update").unwrap();
    assert_ne!(recaptured.digest, captured.digest);
    assert_eq!(recaptured.manifest.sop.version, "2.0.0");
    // The retained bytes are unchanged Tachi-side.
    let retained = client.retained_snapshot(&reference).await.unwrap().unwrap();
    assert_eq!(retained.canonical_digest(), snapshot.canonical_digest());
}

// ── DoD row 4/6: restart / re-admission yields the same content ──────────

#[tokio::test]
async fn restart_replay_yields_same_task_and_same_content() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();

    // The TACHI-side transport (its state survives a ZeroClaw restart —
    // host truth); the ZeroClaw-side client is recreated fresh below,
    // exactly as a process restart would.
    let double = Arc::new(InMemoryTachiTaskBridge::new());
    let client = ProcedureRunClient::new(double.clone(), double.clone());
    let request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "rel-bzip2-1.0.9",
    );
    let first = client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();

    // "Restart": a FRESH ZeroClaw client (no shared state, empty
    // watch cursors) replays the SAME (requester, request_id) tuple —
    // TB-7 rule 2 returns the SAME TaskRef, same binding, and starts no
    // second run. The request id is re-derived deterministically.
    let client2 = ProcedureRunClient::new(double.clone(), double.clone());
    let replayed_request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "rel-bzip2-1.0.9",
    );
    assert_eq!(replayed_request_id.to_string(), request_id.to_string());
    let replay = client2
        .submit_run(
            &snapshot,
            &full_policy(),
            &requester(),
            &replayed_request_id,
        )
        .await
        .unwrap();
    assert_eq!(replay.task_ref, first.task_ref);
    assert!(replay.replayed, "second submit is an idempotent replay");
    assert_eq!(replay.binding, first.binding);
    assert_eq!(double.task_count(), 1);
}

#[tokio::test]
async fn different_digest_same_tuple_is_a_conflict_not_a_second_run() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    let mutated_toml = STAGEX_TOML.replace("1.0.0", "1.0.1");
    write_package(dir.path(), &mutated_toml, STAGEX_MD);
    let mutated = mint_snapshot(&capture_definition(dir.path(), "stagex-update").unwrap()).unwrap();

    let (client, double) = client_and_double();
    let request_id = derive_request_id(&snapshot.procedure_id, &snapshot.procedure_digest, "rel-1");
    client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();
    // Replaying the SAME tuple with a DIFFERENT digest is refused —
    // a mutated definition cannot hijack an existing run binding.
    let replay = client
        .submit_run(&mutated, &full_policy(), &requester(), &request_id)
        .await;
    assert!(matches!(
        replay,
        Err(ProcedureSubmitError::Transport(crate::tachi_bridge::SubmitTransportError))
    ));
    assert_eq!(double.task_count(), 1, "no second task was minted");
}

// ── DoD row 3: publication rules ──────────────────────────────────────────

#[test]
fn draft_revision_mint_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_package(
        dir.path(),
        &STAGEX_TOML.replace("review_state = \"published\"", ""),
        STAGEX_MD,
    );
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    // Absent review_state fails CLOSED to draft.
    assert_eq!(captured.review_state, DefinitionReviewState::Draft);
    assert_eq!(
        mint_snapshot(&captured),
        Err(SnapshotMintError::DraftRevision)
    );

    write_package(
        dir.path(),
        &STAGEX_TOML.replace("published", "draft"),
        STAGEX_MD,
    );
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    assert_eq!(captured.review_state, DefinitionReviewState::Draft);
    assert_eq!(
        mint_snapshot(&captured),
        Err(SnapshotMintError::DraftRevision)
    );

    // An unknown label refuses the CAPTURE (loud authoring failure).
    write_package(
        dir.path(),
        &STAGEX_TOML.replace("published", "publised"),
        STAGEX_MD,
    );
    assert!(capture_definition(dir.path(), "stagex-update").is_err());
}

#[test]
fn mint_performs_no_filesystem_access_after_capture() {
    // Race-free mint: capture, DELETE the tree, then mint — the mint
    // derives purely from the captured bytes (no re-read, so no
    // TOCTOU/mixed-read window can exist).
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    std::fs::remove_dir_all(dir.path()).unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    let recaptured_digest = captured.digest.clone();
    // The embedded bytes re-derive the pinned procedure digest.
    let derived = zeroclaw_api::taskintent::canonical_json_digest_hex(&serde_json::json!({
        "sop_toml": snapshot.definition_toml,
        "sop_md": snapshot.definition_md,
    }));
    assert_eq!(derived, recaptured_digest);
}

// ── DoD row 4: bare-path binding refused ─────────────────────────────────

#[tokio::test]
async fn bare_path_binding_refused() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    // The snapshot ref is ALWAYS content-addressed — never path-shaped.
    let reference = snapshot.snapshot_ref();
    assert!(reference.starts_with(PROCEDURE_SNAPSHOT_REF_PREFIX));
    assert!(!reference.contains('/') && !reference.contains('~'));

    // A submit whose intent binds a definitions-dir PATH instead of the
    // CAS ref is rejected host-side (the carrier binding check).
    let double = Arc::new(InMemoryTachiTaskBridge::new());
    let intent = zeroclaw_api::taskintent::TaskIntentV1 {
        schema: SCHEMA_TAG.to_string(),
        objective: zeroclaw_api::taskintent::BoundedText::new("probe").unwrap(),
        capability_request: zeroclaw_api::taskintent::CapabilityRequest {
            capability: Capability::ReasoningReview,
        },
        requester: requester(),
        parent_ref: None,
        supervisor_ref: None,
        context_bundle_ref: zeroclaw_api::taskintent::BoundedText::new(
            "/sops/stagex-update/SOP.toml",
        )
        .unwrap(),
        source_refs: vec![],
        constraints: vec![],
        expected_artifacts: vec![],
        evaluation_requirement: zeroclaw_api::taskintent::EvaluationRequirement {
            independence: zeroclaw_api::taskintent::IndependenceClass::DeterministicCheck,
        },
        workspace_source: None,
        routing_preference: None,
        approval_requirement: ApprovalRequirement::NotRequired,
        privacy_class: PrivacyClass::Public,
        expiry: None,
        retry_of: None,
    };
    let request_id = RequestId::try_from("probe-bare-path".to_string()).unwrap();
    let receipt = double
        .submit_procedure_run(&intent, &request_id, &snapshot)
        .await
        .unwrap();
    assert!(
        matches!(receipt, SubmitReceipt::Rejected { ref reason } if reason == "snapshot_ref_binding_mismatch"),
        "bare-path binding refused: {receipt:?}"
    );
}

// ── DoD row 9: bounded carrier + narrow-only capability ──────────────────

#[test]
fn oversize_snapshot_is_a_typed_refusal() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let mut oversized = captured.clone();
    oversized.md_bytes = format!(
        "## Steps\n\n1. **Fill** — {}\n",
        "x".repeat(zeroclaw_api::procedure_v1::PROCEDURE_SNAPSHOT_MAX_BYTES)
    );
    // Make the steps list match so the projection stays coherent.
    oversized.steps = vec![crate::sop::types::SopStep {
        number: 1,
        title: "Fill".into(),
        body: String::new(),
        ..Default::default()
    }];
    assert!(matches!(
        mint_snapshot(&oversized),
        Err(SnapshotMintError::Oversize { .. })
    ));
}

#[tokio::test]
async fn narrow_only_capability_refusal() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    // The stagex fixture names shell/file_write tools → it requires
    // RepositoryImplementation.
    assert_eq!(
        snapshot.guidance.required_capability,
        Capability::RepositoryImplementation
    );

    // A read-only requester policy cannot run it: guidance can only
    // NARROW, never originate/widen capability.
    let read_only = RequesterBridgePolicy {
        admitted_capabilities: BTreeSet::from([Capability::ReadOnlyInvestigation]),
        workspace_source: None,
        routing_preference: None,
        approval_requirement: ApprovalRequirement::NotRequired,
        privacy_class: PrivacyClass::Public,
    };
    let (client, _double) = client_and_double();
    let request_id = derive_request_id("stagex-update", &snapshot.procedure_digest, "rel-1");
    let refused = client
        .submit_run(&snapshot, &read_only, &requester(), &request_id)
        .await;
    assert!(matches!(
        refused,
        Err(ProcedureSubmitError::CapabilityNotAdmitted {
            required: Capability::RepositoryImplementation
        })
    ));
}

// ── DoD row 10: privacy (existence-blind refusal) ────────────────────────

#[test]
fn private_dyad_content_refused_existence_blind() {
    let marker = "Private Dyad notes belong here";
    let dir = tempfile::tempdir().unwrap();
    write_package(
        dir.path(),
        STAGEX_TOML,
        &format!("{STAGEX_MD}\n\n> {marker}\n"),
    );
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let refusal = mint_snapshot(&captured).unwrap_err();
    match refusal {
        SnapshotMintError::ForbiddenContent { category } => {
            assert_eq!(category, SnapshotContentCategory::PrivateDyad);
        }
        other => panic!("expected PrivateDyad refusal, got {other:?}"),
    }
    // Existence-blind: the refusal error carries the CATEGORY, never the
    // material, its ids, or its extent.
    let text = format!("{refusal}");
    assert!(
        !text.contains(marker),
        "refusal echoes Private Dyad material"
    );
    assert!(
        !text.contains("Dyad notes"),
        "refusal echoes Private Dyad content shape"
    );
    assert_eq!(text.matches("private_dyad").count(), 1);
}

#[test]
fn credential_content_refused_at_mint() {
    let dir = tempfile::tempdir().unwrap();
    // The fixture token is built by concat so the literal never appears
    // in source (gitleaks/GitHub push protection pattern).
    let fixture_token = format!("{}{}", "ghp_", "abcdef123456");
    write_package(
        dir.path(),
        STAGEX_TOML,
        &format!("{STAGEX_MD}\n\n- token: {fixture_token}\n"),
    );
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    assert!(matches!(
        mint_snapshot(&captured),
        Err(SnapshotMintError::ForbiddenContent {
            category: SnapshotContentCategory::Credential
        })
    ));
}

// ── DoD row 7: the KP-15 binding ─────────────────────────────────────────

#[tokio::test]
async fn run_binding_carries_procedure_identity_not_run_counters() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    let (client, _double) = client_and_double();
    let request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "instance-42",
    );
    let output = client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();
    // KP-15 tuple: (procedure_id, revision, digest, snapshot_ref) — the
    // definition revision is the manifest version "1.0.0", never a
    // run-state CAS counter (which would start at 0/1 and increment per
    // write; a second submit replays the same binding).
    assert_eq!(output.binding.procedure_id, "stagex-update");
    assert_eq!(output.binding.revision, "1.0.0");
    assert_eq!(output.binding.digest, snapshot.procedure_digest);
    let second = client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();
    assert_eq!(second.binding, output.binding);
}

// ── Approval lane: gates park, decisions land host-side ──────────────────

#[tokio::test]
async fn approval_gate_parks_and_resolves_through_host_side_decision() {
    let gated_md = STAGEX_MD.replacen(
        "   - tools: shell, file_read\n",
        "   - tools: shell, file_read\n   - requires_confirmation: true\n",
        1,
    );
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path(), STAGEX_TOML, &gated_md);
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    assert_eq!(snapshot.approval_gates.len(), 1);
    assert_eq!(snapshot.approval_gates[0].step, 1);
    assert_eq!(snapshot.guidance.user_input_points, vec![1]);

    let (client, double) = client_and_double();
    let request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "gated-1",
    );
    let output = client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();
    let reference = snapshot.snapshot_ref();

    // Drive: parks at gate 1, zero steps completed.
    let (completed, gate) = double.drive_procedure_steps(&output.task_ref, &reference);
    assert_eq!(completed, Vec::<u32>::new());
    assert_eq!(gate, Some(1));

    // The decision is recorded TACHI-side, idempotently; a re-used
    // decision id replays, a different id on a resolved gate refuses.
    double
        .resolve_procedure_gate(&output.task_ref, 1, "approve", "dec-1")
        .unwrap();
    double
        .resolve_procedure_gate(&output.task_ref, 1, "approve", "dec-1")
        .unwrap();
    assert!(
        double
            .resolve_procedure_gate(&output.task_ref, 1, "deny", "dec-2")
            .is_err()
    );
    assert!(
        double
            .resolve_procedure_gate(&output.task_ref, 1, "maybe", "dec-3")
            .is_err()
    );

    // Resumed: all 8 steps complete from the retained snapshot.
    let (completed, gate) = double.drive_procedure_steps(&output.task_ref, &reference);
    assert_eq!(completed, (1..=8).collect::<Vec<u32>>());
    assert_eq!(gate, None);
    // The decision is on the durable fact log (host-side truth).
    assert_eq!(
        double.procedure_gate_decisions(&output.task_ref),
        vec![(1u32, "approve".to_string(), "dec-1".to_string())]
    );
}

// ── DoD row 12: candidate-only learning output ───────────────────────────

#[tokio::test]
async fn learning_output_is_candidate_only_with_no_apply_path() {
    let dir = fixture_dir();
    let captured = capture_definition(dir.path(), "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    let (client, double) = client_and_double();
    let request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "learn-1",
    );
    let output = client
        .submit_run(&snapshot, &full_policy(), &requester(), &request_id)
        .await
        .unwrap();
    double.drive_procedure_steps(&output.task_ref, &snapshot.snapshot_ref());
    double.ingest_execution(&output.task_ref, "completed");
    let attempt: zeroclaw_api::taskintent::AttemptRef =
        serde_json::from_value(serde_json::Value::String("attempt:proc-1".to_string()))
            .expect("wire-shaped attempt ref");
    double.observe_outcome(
        &output.task_ref,
        attempt,
        "completed",
        Some("artifact://run-report".to_string()),
        vec!["evidence://run-1".to_string()],
        true,
        false,
        "procedure-controller",
    );
    double.ingest_adjudication(&output.task_ref, "accepted");
    let projection = client.collect_latest(&output.task_ref).await.unwrap();

    let candidate =
        super::run::derive_learning_candidate(&snapshot, &output.task_ref, &projection, None);
    // Routes into the reviewed-promotion path — the SAME single review
    // surface, and the kind requires review (no apply).
    let proposed = candidate.to_proposed_candidate();
    assert_eq!(proposed.kind, ProposedCandidateKind::Procedure);
    assert!(proposed.kind.requires_reviewed_promotion());
    assert!(proposed.is_substantiated());
    // Sensitive derivations carry derivation_ref (KP-19).
    let sensitive = super::run::derive_learning_candidate(
        &snapshot,
        &output.task_ref,
        &projection,
        Some("evidence://sensitive-derivation"),
    );
    assert_eq!(
        sensitive.derivation_ref.as_deref(),
        Some("evidence://sensitive-derivation")
    );
}

// ── No local run store: source-level discrimination ──────────────────────

#[test]
fn procedure_v1_module_has_no_durable_write_calls() {
    // The module's own source (this crate's files) must contain no
    // std::fs::write / create_dir_all / File::create calls — the KP-16
    // law is structural. The full BEHAVIORAL discrimination (hostile
    // legacy settings, unchanged runs.db) lives in the integration
    // binary `procedure_v1_no_durable_writes.rs`.
    let files = [
        "src/procedure_v1/mod.rs",
        "src/procedure_v1/definition.rs",
        "src/procedure_v1/snapshot.rs",
        "src/procedure_v1/run.rs",
    ];
    for file in files {
        let source =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
                .unwrap();
        for forbidden in [
            "fs::write",
            "fs::create_dir_all",
            "File::create",
            "OpenOptions::new",
            "SopEngine",
            "SopRunStore",
        ] {
            assert!(
                !source.contains(forbidden),
                "{file} contains forbidden durable-write/legacy-engine token `{forbidden}`"
            );
        }
    }
}
