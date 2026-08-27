//! KP-16 discrimination, procedure half (vertical V4): the V4
//! procedure-run path creates NO durable state anywhere in the
//! ZeroClaw-side filesystem — not under the definitions tree (no
//! `*.state.json` or successor run-state file), no `runs.db`, no
//! second ledger of any kind. Durable run truth stays Tachi-side
//! through the bridge port.
//!
//! This test runs the FULL V4 flow (capture → mint → submit →
//! step-drive → outcome/adjudication → collect → candidate) under
//! HOSTILE conditions — a definitions tree that the legacy engine
//! would happily write run state into (and a pre-existing `runs.db`
//! the legacy store would append to) — and proves the tree and the
//! database are byte-identical afterwards.
//!
//! The working transport double lives in this test binary (the
//! in-memory double in `tachi_bridge` is `pub(crate)`/`cfg(test)`,
//! invisible to integration binaries): a miniature of the same host
//! law — admission, TB-7 tuple binding, CAS retention with
//! verify-before-ack, step-driving strictly from retained bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zeroclaw_api::procedure_v1::{PROCEDURE_SNAPSHOT_MAX_BYTES, ProcedureSnapshotV1};
use zeroclaw_api::taskintent::{
    AttemptRef, InterventionError, InterventionReceipt, InterventionV1, RequestId, RequesterRef,
    StopMode, StopReceipt, TaskIntentV1, TaskRef,
};

use zeroclaw_runtime::procedure_v1::{
    ProcedureRunClient, capture_definition, derive_learning_candidate, derive_request_id,
    mint_snapshot,
};
use zeroclaw_runtime::tachi_bridge::compose::{RequesterBridgePolicy, scan_intent};
use zeroclaw_runtime::tachi_bridge::procedure::ProcedureSubmitPort;
use zeroclaw_runtime::tachi_bridge::{
    BridgeQueryError, ResultProjectionView, SubmitReceipt, SubmitTransportError, TachiTaskBridge,
    TaskEventPageView, TaskEventView, TaskSnapshotView,
};

const STAGEX_TOML: &str = r#"[sop]
name = "stagex-update"
description = "Watch the upstream release feed, bump the StageX package, build it, verify it reproduces by digest, push the change, open a draft pull request, and announce the result."
version = "1.0.0"
deterministic = true
review_state = "published"

[[triggers]]
type = "manual"
"#;

const STAGEX_MD: &str = r#"## Steps

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

// ─── a compact working transport double (host law miniature) ─────────────

#[derive(Default)]
struct HostHalf {
    bindings: BTreeMap<(String, String), (String, TaskRef)>,
    next_task: u64,
    facts: BTreeMap<String, Vec<TaskEventView>>,
    snapshots: BTreeMap<String, ProcedureSnapshotV1>,
    outcomes: BTreeMap<String, (AttemptRef, Vec<String>, bool)>,
    adjudication: BTreeMap<String, String>,
}

impl HostHalf {
    fn mint_task_ref(&mut self) -> TaskRef {
        self.next_task += 1;
        serde_json::from_value(serde_json::Value::String(format!(
            "task:v4int-{:08x}",
            self.next_task
        )))
        .expect("minted ref is wire-shaped")
    }

    fn append(&mut self, task_ref: &TaskRef, kind: &str) {
        let log = self
            .facts
            .entry(task_ref.as_wire().to_string())
            .or_default();
        let seq = log.len() as u64 + 1;
        log.push(TaskEventView {
            seq,
            event_id: format!("ev-{seq}-{kind}"),
            source: "procedure-controller".to_string(),
            source_revision: "1".to_string(),
            occurred_at: "2026-08-26T00:00:00Z".to_string(),
            recorded_at: "2026-08-26T00:00:00Z".to_string(),
            payload_digest: format!("{kind}-{seq}"),
            visibility: "internal".to_string(),
            kind: kind.to_string(),
        });
    }
}

#[derive(Default, Clone)]
struct WorkingTransport {
    host: Arc<Mutex<HostHalf>>,
}

#[async_trait]
impl TachiTaskBridge for WorkingTransport {
    async fn submit(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        if let Err(rejection) = scan_intent(intent) {
            return Ok(SubmitReceipt::Rejected {
                reason: rejection.to_string(),
            });
        }
        let digest = intent.canonical_digest();
        let mut host = self.host.lock();
        let tuple = (intent.requester.to_string(), request_id.to_string());
        if let Some((bound_digest, task_ref)) = host.bindings.get(&tuple) {
            if *bound_digest != digest {
                return Ok(SubmitReceipt::RequestIdConflict {
                    bound_digest: bound_digest.clone(),
                    submitted_digest: digest,
                });
            }
            return Ok(SubmitReceipt::Admitted {
                task_ref: task_ref.clone(),
                replayed: true,
            });
        }
        let task_ref = host.mint_task_ref();
        host.bindings.insert(tuple, (digest, task_ref.clone()));
        host.append(&task_ref, "task_submitted");
        Ok(SubmitReceipt::Admitted {
            task_ref,
            replayed: false,
        })
    }

    async fn get(&self, task_ref: &TaskRef) -> Result<TaskSnapshotView, BridgeQueryError> {
        let host = self.host.lock();
        if !host.facts.contains_key(task_ref.as_wire()) {
            return Err(BridgeQueryError::NotFound);
        }
        let completed = host
            .facts
            .get(task_ref.as_wire())
            .is_some_and(|log| log.iter().any(|event| event.kind == "outcome_observed"));
        Ok(TaskSnapshotView {
            task_ref: task_ref.clone(),
            task_revision: host.facts.get(task_ref.as_wire()).map_or(0, Vec::len) as u64,
            execution: zeroclaw_runtime::tachi_bridge::ProjectedExecutionState::project(
                if completed { "completed" } else { "running" },
            )
            .expect("mapped label"),
            adjudication: zeroclaw_runtime::tachi_bridge::ProjectedAdjudicationState::project(
                host.adjudication
                    .get(task_ref.as_wire())
                    .map(String::as_str)
                    .unwrap_or("unreviewed"),
            )
            .expect("mapped label"),
            delivery: zeroclaw_runtime::tachi_bridge::ProjectedDeliveryState::project(
                if host.outcomes.contains_key(task_ref.as_wire()) {
                    "ready"
                } else {
                    "not_ready"
                },
            )
            .expect("mapped label"),
            lifecycle_mode: Some("tachi_managed_batch".to_string()),
            intent_digest: String::new(),
        })
    }

    async fn watch(
        &self,
        task_ref: &TaskRef,
        after_seq: u64,
        limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        let host = self.host.lock();
        let Some(log) = host.facts.get(task_ref.as_wire()) else {
            return Err(BridgeQueryError::NotFound);
        };
        let events = log
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit)
            .cloned()
            .collect();
        Ok(TaskEventPageView {
            task_ref: task_ref.clone(),
            events,
            has_more: false,
        })
    }

    async fn collect(
        &self,
        task_ref: &TaskRef,
        _result_revision: Option<u64>,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        let host = self.host.lock();
        let Some((attempt, evidence, verification)) = host.outcomes.get(task_ref.as_wire()) else {
            return Err(BridgeQueryError::NotReady);
        };
        Ok(ResultProjectionView {
            task_ref: task_ref.clone(),
            attempt_ref: Some(attempt.clone()),
            terminal_classification: "success".to_string(),
            canonical_artifact_ref: Some("artifact://procedure-run-report".to_string()),
            artifact_evidence_refs: evidence.clone(),
            verification: zeroclaw_runtime::tachi_bridge::VerificationSummaryView {
                verification_present: *verification,
                diff_present: false,
                evidence_ref_count: evidence.len(),
            },
            adjudication: zeroclaw_runtime::tachi_bridge::ProjectedAdjudicationState::project(
                host.adjudication
                    .get(task_ref.as_wire())
                    .map(String::as_str)
                    .unwrap_or("unreviewed"),
            )
            .expect("mapped label"),
            contract_violations: vec![],
            provenance: "procedure-controller".to_string(),
            pending_user_action: None,
            result_revision: 1,
        })
    }

    async fn intervene(
        &self,
        _task_ref: &TaskRef,
        _intervention: &InterventionV1,
        _requester: &RequesterRef,
        _request_id: &RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError> {
        Err(InterventionError::UnsupportedByLifecycleOwner {
            operation: zeroclaw_api::taskintent::InterventionStatic::Escalate,
        })
    }

    async fn request_stop(
        &self,
        _task_ref: &TaskRef,
        _mode: StopMode,
        _requester: &RequesterRef,
        _request_id: &RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError> {
        Err(InterventionError::UnsupportedByLifecycleOwner {
            operation: zeroclaw_api::taskintent::InterventionStatic::RequestHardCancel,
        })
    }
}

#[async_trait]
impl ProcedureSubmitPort for WorkingTransport {
    async fn submit_procedure_run(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
        snapshot: &ProcedureSnapshotV1,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        // Verify-before-ack: size, digest self-consistency, ref binding.
        if snapshot.serialized_len() > PROCEDURE_SNAPSHOT_MAX_BYTES {
            return Ok(SubmitReceipt::Rejected {
                reason: "snapshot_oversize".to_string(),
            });
        }
        let digest = snapshot.canonical_digest();
        if snapshot.snapshot_ref() != format!("proceduresnap:{digest}") {
            return Ok(SubmitReceipt::Rejected {
                reason: "snapshot_digest_mismatch".to_string(),
            });
        }
        if intent.context_bundle_ref.as_str() != snapshot.snapshot_ref() {
            return Ok(SubmitReceipt::Rejected {
                reason: "snapshot_ref_binding_mismatch".to_string(),
            });
        }
        self.host.lock().snapshots.insert(digest, snapshot.clone());
        self.submit(intent, request_id).await
    }

    async fn retained_snapshot(
        &self,
        snapshot_ref: &str,
    ) -> Result<Option<ProcedureSnapshotV1>, SubmitTransportError> {
        let digest = snapshot_ref
            .strip_prefix("proceduresnap:")
            .unwrap_or_default();
        Ok(self.host.lock().snapshots.get(digest).cloned())
    }
}

impl WorkingTransport {
    fn drive_steps(&self, task_ref: &TaskRef, snapshot_ref: &str) {
        let snapshot = {
            let host = self.host.lock();
            let digest = snapshot_ref
                .strip_prefix("proceduresnap:")
                .unwrap_or_default();
            host.snapshots.get(digest).cloned()
        }
        .expect("retained snapshot");
        for _ in &snapshot.steps {
            self.host
                .lock()
                .append(task_ref, "procedure_step_completed");
        }
        self.host.lock().append(task_ref, "outcome_observed");
        let attempt: AttemptRef =
            serde_json::from_value(serde_json::Value::String("attempt:v4-1".to_string()))
                .expect("wire-shaped");
        self.host.lock().outcomes.insert(
            task_ref.as_wire().to_string(),
            (attempt, vec!["evidence://v4-run".to_string()], true),
        );
        self.host
            .lock()
            .adjudication
            .insert(task_ref.as_wire().to_string(), "accepted".to_string());
    }
}

// ─── hostile tree snapshotting ────────────────────────────────────────────

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.insert(path, bytes);
            }
        }
    }
    out
}

fn tree_diff(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> Vec<String> {
    let mut diff = Vec::new();
    for (path, bytes) in after {
        match before.get(path) {
            Some(old) if old == bytes => {}
            _ => diff.push(format!("changed/new: {}", path.display())),
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            diff.push(format!("deleted: {}", path.display()));
        }
    }
    diff
}

// ─── the discrimination ───────────────────────────────────────────────────

#[tokio::test]
async fn v4_procedure_run_writes_no_durable_state_under_hostile_legacy_layout() {
    // Hostile layout: definitions tree + a legacy-named runs.db that a
    // legacy engine/store WOULD have written into. The V4 path must
    // leave every byte identical.
    let dir = tempfile::tempdir().unwrap();
    let sops = dir.path().join("sops");
    let package = sops.join("stagex-update");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("SOP.toml"), STAGEX_TOML).unwrap();
    std::fs::write(package.join("SOP.md"), STAGEX_MD).unwrap();
    let legacy_db = dir.path().join("sop").join("runs.db");
    std::fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
    std::fs::write(&legacy_db, b"legacy-bytes-must-stay-identical").unwrap();

    let before = snapshot_tree(dir.path());

    // The full V4 flow.
    let captured = capture_definition(&sops, "stagex-update").unwrap();
    let snapshot = mint_snapshot(&captured).unwrap();
    let transport = Arc::new(WorkingTransport::default());
    let client = ProcedureRunClient::new(transport.clone(), transport.clone());
    let policy = RequesterBridgePolicy {
        admitted_capabilities: BTreeSet::from([
            zeroclaw_api::taskintent::Capability::RepositoryImplementation,
        ]),
        workspace_source: None,
        routing_preference: None,
        approval_requirement: zeroclaw_api::taskintent::ApprovalRequirement::NotRequired,
        privacy_class: zeroclaw_api::taskintent::PrivacyClass::Public,
    };
    let requester =
        RequesterRef::try_from("requester:zeroclaw-procedure-v4".to_string()).expect("bounded");
    let request_id = derive_request_id(
        &snapshot.procedure_id,
        &snapshot.procedure_digest,
        "rel-bzip2-1.0.9",
    );
    let output = client
        .submit_run(&snapshot, &policy, &requester, &request_id)
        .await
        .unwrap();
    assert!(!output.replayed);

    // Tachi-side drive: steps from the RETAINED bytes, outcome, eval.
    transport.drive_steps(&output.task_ref, &snapshot.snapshot_ref());

    // Projection + candidate — consumed through refs only.
    let projection = client.collect_latest(&output.task_ref).await.unwrap();
    assert_eq!(projection.terminal_classification, "success");
    let candidate = derive_learning_candidate(&snapshot, &output.task_ref, &projection, None);
    assert!(
        candidate
            .to_proposed_candidate()
            .kind
            .requires_reviewed_promotion()
    );

    // "Restart": fresh client over the same host truth.
    let client2 = ProcedureRunClient::new(transport.clone(), transport.clone());
    let replay = client2
        .submit_run(&snapshot, &policy, &requester, &request_id)
        .await
        .unwrap();
    assert_eq!(replay.task_ref, output.task_ref);
    assert!(replay.replayed);

    // THE DISCRIMINATION: the whole tree is byte-identical — no
    // *.state.json under sops/, no runs.db growth, no new files
    // anywhere in the workspace root this flow was pointed at.
    let after = snapshot_tree(dir.path());
    let diff = tree_diff(&before, &after);
    assert!(
        diff.is_empty(),
        "V4 procedure-run flow changed the filesystem: {diff:?}"
    );

    // Explicit named checks (KP-16 owner test 3 shape).
    assert!(package.join("SOP.toml").exists());
    let stray_state: Vec<_> = after
        .keys()
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(".state.json"))
        })
        .collect();
    assert!(
        stray_state.is_empty(),
        "run-state files appeared: {stray_state:?}"
    );
    assert_eq!(
        std::fs::read(&legacy_db).unwrap(),
        b"legacy-bytes-must-stay-identical",
        "legacy runs.db was touched"
    );
}
