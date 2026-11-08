//! Temporary, ignored Rust profile for audit F09. This module is not a product
//! benchmark framework and must not be included in the product PR.

use super::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use zeroclaw_api::TOOL_LOOP_MEMORY_SESSIONS;

const AGENT: &str = "synthetic-profile-agent";
const SESSION: &str = "synthetic-profile-session";
const TARGETS: [usize; 3] = [1_000, 10_000, 50_000];
const BODY_BYTES: usize = 1024;
const STAMP: &str = "2026-01-01T00:00:00.000Z";
const SAMPLES: usize = 9;
const FORGET_KEY: &str = "synthetic-forget-target";
const NEW_WRITE_KEY: &str = "synthetic-new-write-target";

#[derive(Clone, Copy)]
enum Strategy {
    LegacyPrefix,
    Current,
}

struct LookupCase {
    name: &'static str,
    key: &'static str,
    host_scope: bool,
    expected_row: Option<usize>,
}

const CASES: [LookupCase; 5] = [
    LookupCase {
        name: "canonical_conversation_hit",
        key: "synthetic-session-hit",
        host_scope: true,
        expected_row: Some(0),
    },
    LookupCase {
        name: "canonical_global_without_host_scope",
        key: "synthetic-global-hit",
        host_scope: false,
        expected_row: Some(1),
    },
    LookupCase {
        name: "global_in_host_scope_legacy_override_check",
        key: "synthetic-global-hit",
        host_scope: true,
        expected_row: Some(1),
    },
    LookupCase {
        name: "noncanonical_legacy_conversation_hit",
        key: "synthetic-legacy-hit",
        host_scope: true,
        expected_row: Some(2),
    },
    LookupCase {
        name: "new_key_miss_lookup_only",
        key: "synthetic-unwritten-key",
        host_scope: true,
        expected_row: None,
    },
];

#[derive(Default)]
struct CorpusTotals {
    rows: usize,
    text_bytes: usize,
    summary_bytes: usize,
    input_metadata_bytes: usize,
}

fn row_id(index: usize) -> String {
    format!("synthetic-profile-row-{index:08}")
}

fn payload(index: usize) -> String {
    let mut body = format!("Synthetic F09 profile fixture. Corpus row {index:08}. ");
    body.push_str(&"memory lookup profile payload ".repeat(BODY_BYTES / 16));
    body.truncate(BODY_BYTES);
    body
}

fn corpus_entry(index: usize) -> portable_kernel::MemoryEntry {
    let (key, category, session) = match index {
        0 => (
            CASES[0].key.to_owned(),
            MemoryCategory::Conversation,
            Some(SESSION),
        ),
        1 => (CASES[1].key.to_owned(), MemoryCategory::Core, None),
        2 => (
            CASES[3].key.to_owned(),
            MemoryCategory::Conversation,
            Some(SESSION),
        ),
        3 => (FORGET_KEY.to_owned(), MemoryCategory::Core, None),
        _ => (
            format!("synthetic-background-{index:08}"),
            MemoryCategory::Core,
            None,
        ),
    };
    let path = if index == 2 {
        format!(
            "{}legacy/opaque-profile-row",
            TachiMemory::agent_path_prefix(Some(AGENT))
        )
    } else {
        TachiMemory::scoped_storage_path(Some(AGENT), None, &category, &key, session)
    };
    let text = payload(index);
    portable_kernel::MemoryEntry {
        id: row_id(index),
        path,
        summary: text.chars().take(100).collect(),
        text,
        importance: 0.7,
        timestamp: "2026-01-01T00:00:00Z".into(),
        valid_from: "2026-01-01T00:00:00Z".into(),
        valid_until: None,
        category: TachiMemory::category_to_memcore(&category),
        topic: String::new(),
        keywords: Vec::new(),
        persons: Vec::new(),
        entities: Vec::new(),
        location: String::new(),
        source: "zeroclaw".into(),
        scope: "general".into(),
        archived: false,
        access_count: 0,
        scored_count: 0,
        last_use_at: None,
        last_access: None,
        revision: 1,
        vector: None,
        retention_policy: None,
        domain: None,
        metadata: TachiMemory::build_metadata(
            &key,
            &category,
            session,
            None,
            Some(AGENT),
            &StoreOptions::default(),
        ),
        recall_count: 0,
        query_diversity: 0,
        tier: portable_kernel::types::default_tier(),
    }
}

fn snapshot_entry(index: usize) -> portable_kernel::PortableImportEntry {
    let mut entry = corpus_entry(index);
    // Ordinary upsert normalizes these fixture fields; snapshot import must
    // receive their stored values. The preflight below verifies every field
    // against ordinary upsert readback rather than assuming equivalence.
    entry.source = portable_kernel::types::MemorySource::parse_or_external(&entry.source);
    entry.timestamp = STAMP.into();
    entry.valid_from = STAMP.into();
    portable_kernel::PortableImportEntry {
        entry,
        created_at: STAMP.into(),
        updated_at: STAMP.into(),
        superseded_by: None,
    }
}

fn verify_snapshot_fixture_parity() -> anyhow::Result<Value> {
    let directory = tempfile::TempDir::new()?;
    let control = TachiMemory::new("synthetic-profile-preflight", directory.path())?;
    // Cover each distinct row shape and all corpus index widths through the
    // genuine prior seed API. This control store is never used for timing.
    let indices = [0, 1, 2, 3, 4, 999, 9_999, 49_999];
    let entries: Vec<_> = indices.into_iter().map(corpus_entry).collect();
    let mut store = control.store_handle().lock();
    store
        .upsert_batch(&entries)
        .map_err(|error| anyhow::Error::msg(format!("profile parity upsert: {error}")))?;
    for index in indices {
        let expected = snapshot_entry(index).entry;
        let actual = store
            .get(&expected.id)
            .map_err(|error| anyhow::Error::msg(format!("profile parity get: {error}")))?
            .context("profile parity row is missing")?;
        anyhow::ensure!(
            serde_json::to_value(actual)? == serde_json::to_value(expected)?,
            "snapshot fixture differs from ordinary upsert readback at {index}"
        );
    }
    Ok(
        json!({"rows":indices.len(),"compared":"all serialized MemoryEntry fields, including full metadata",
        "source":"ordinary typed upsert_batch followed by typed get","in_operation_timing":false}),
    )
}

fn seed_snapshot(memory: &TachiMemory, target: usize) -> anyhow::Result<(CorpusTotals, Value)> {
    let entries: Vec<_> = (0..target).map(snapshot_entry).collect();
    let mut totals = CorpusTotals::default();
    for import in &entries {
        totals.text_bytes += import.entry.text.len();
        totals.summary_bytes += import.entry.summary.len();
        totals.input_metadata_bytes += serde_json::to_vec(&import.entry.metadata)?.len();
    }
    totals.rows = target;
    let expected_lifecycle =
        portable_kernel::PortableImportReceipt::expected_lifecycle_checksum(&entries)
            .map_err(|error| anyhow::Error::msg(format!("profile lifecycle checksum: {error}")))?;
    let expected_vectors =
        portable_kernel::PortableImportReceipt::expected_vector_checksum(&entries)
            .map_err(|error| anyhow::Error::msg(format!("profile vector checksum: {error}")))?;
    let mut store = memory.store_handle().lock();
    // One typed import into this size's fresh admitted MemoryStore. The kernel
    // itself proves the target/projections empty under its transaction before
    // omitting redundant FTS deletes; no raw connection, SQL or new owner.
    let import_start = Instant::now();
    let receipt = store
        .import_snapshot_batch(&entries)
        .map_err(|error| anyhow::Error::msg(format!("profile snapshot seed: {error}")))?;
    let import_ms = import_start.elapsed().as_secs_f64() * 1000.0;
    anyhow::ensure!(
        receipt.rows_imported == target
            && receipt.vectors_imported == 0
            && receipt.vectors_absent == target
            && receipt.dangling_supersessions.is_empty()
            && receipt.lifecycle_checksum == expected_lifecycle
            && receipt.vector_checksum == expected_vectors,
        "profile snapshot receipt mismatch"
    );
    let verify_start = Instant::now();
    for import in &entries {
        let actual = store
            .get(&import.entry.id)
            .map_err(|error| anyhow::Error::msg(format!("profile snapshot readback: {error}")))?
            .context("profile snapshot row is missing")?;
        anyhow::ensure!(
            serde_json::to_value(actual)? == serde_json::to_value(&import.entry)?,
            "profile snapshot full-field readback mismatch: {}",
            import.entry.id
        );
    }
    let verification_ms = verify_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TACHI_PROFILE_SEED rows={target} import_ms={import_ms:.3} verification_ms={verification_ms:.3}"
    );
    Ok((
        totals,
        json!({"import_ms":import_ms,"full_field_readback_ms":verification_ms,
        "verified_rows":target,"receipt":receipt}),
    ))
}

/// Pre-F09 point selection from the K01 parent: invoke the unchanged production
/// growing prefix scanner, filter by logical key, then apply the same PointScope.
/// This compares query strategies without reintroducing the old session bug.
async fn legacy_selected_rows(memory: &TachiMemory, key: &str) -> anyhow::Result<Vec<MemoryEntry>> {
    let scope = PointScope::current();
    let store = memory.store.clone();
    let key = key.to_owned();
    let prefix = TachiMemory::agent_path_prefix(Some(AGENT));
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
        let store = store.lock();
        let rows = TachiMemory::list_prefix_complete(
            &store,
            &prefix,
            DEFAULT_LIST_PAGE_SIZE,
            MAX_LIST_GROW,
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                TachiMemory::metadata_str(&row.metadata, METADATA_ZC_KEY).as_deref()
                    == Some(key.as_str())
            })
            .map(|row| TachiMemory::memcore_to_zeroclaw(row, None))
            .collect())
    })
    .await??;
    Ok(scope.select(rows, true))
}

async fn read_one(
    memory: &TachiMemory,
    case: &LookupCase,
    strategy: Strategy,
) -> anyhow::Result<Option<MemoryEntry>> {
    let operation = async {
        match strategy {
            Strategy::LegacyPrefix => Ok(legacy_selected_rows(memory, case.key).await?.pop()),
            Strategy::Current => memory.get_for_agent(case.key, AGENT).await,
        }
    };
    if case.host_scope {
        TOOL_LOOP_MEMORY_SESSIONS
            .scope(vec![SESSION.into()], operation)
            .await
    } else {
        operation.await
    }
}

fn elapsed_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn check_read(case: &LookupCase, entry: Option<&MemoryEntry>) -> anyhow::Result<()> {
    let expected_id = case.expected_row.map(row_id);
    anyhow::ensure!(
        entry.map(|row| row.id.as_str()) == expected_id.as_deref(),
        "profile result id mismatch: {}",
        case.name
    );
    if let Some(entry) = entry {
        anyhow::ensure!(
            entry.key == case.key && entry.content.len() == BODY_BYTES,
            "profile row mismatch: {}",
            case.name
        );
    }
    Ok(())
}

fn sample_summary(samples: &[u64]) -> Value {
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let p95 = (ordered.len() * 95).div_ceil(100) - 1;
    let ms = |ns: u64| Duration::from_nanos(ns).as_secs_f64() * 1000.0;
    json!({
        "samples_ns": samples,
        "sample_count": samples.len(),
        "min_ms": ms(ordered[0]),
        "median_ms": ms(ordered[ordered.len() / 2]),
        "p95_nearest_rank_ms": ms(ordered[p95]),
        "max_ms": ms(ordered[ordered.len() - 1]),
    })
}

async fn measure_lookup(memory: &TachiMemory, case: &LookupCase) -> anyhow::Result<Value> {
    for strategy in [Strategy::LegacyPrefix, Strategy::Current] {
        check_read(case, read_one(memory, case, strategy).await?.as_ref())?;
    }
    let mut old = Vec::with_capacity(SAMPLES);
    let mut current = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let order = if index % 2 == 0 {
            [Strategy::LegacyPrefix, Strategy::Current]
        } else {
            [Strategy::Current, Strategy::LegacyPrefix]
        };
        for strategy in order {
            let start = Instant::now();
            let result = read_one(memory, case, strategy).await?;
            let ns = elapsed_ns(start);
            check_read(case, result.as_ref())?;
            match strategy {
                Strategy::LegacyPrefix => old.push(ns),
                Strategy::Current => current.push(ns),
            }
        }
    }
    Ok(json!({"case":case.name,"host_scope":case.host_scope,
        "legacy_prefix_get":sample_summary(&old),"current_get_for_agent":sample_summary(&current)}))
}

async fn forget_one(memory: &TachiMemory, strategy: Strategy) -> anyhow::Result<bool> {
    TOOL_LOOP_MEMORY_SESSIONS
        .scope(vec![SESSION.into()], async {
            match strategy {
                Strategy::LegacyPrefix => {
                    let ids = legacy_selected_rows(memory, FORGET_KEY)
                        .await?
                        .into_iter()
                        .map(|row| row.id)
                        .collect();
                    Ok(memory.delete_ids(ids).await? > 0)
                }
                Strategy::Current => memory.forget_for_agent(FORGET_KEY, AGENT).await,
            }
        })
        .await
}

async fn measure_forget(memory: &TachiMemory) -> anyhow::Result<Value> {
    let restore = || {
        memory
            .store_handle()
            .lock()
            .upsert(&corpus_entry(3))
            .map_err(|error| anyhow::Error::msg(format!("profile restore: {error}")))
    };
    let mut old = Vec::with_capacity(SAMPLES);
    let mut current = Vec::with_capacity(SAMPLES);
    for index in 0..=SAMPLES {
        let order = if index % 2 == 0 {
            [Strategy::LegacyPrefix, Strategy::Current]
        } else {
            [Strategy::Current, Strategy::LegacyPrefix]
        };
        for strategy in order {
            let start = Instant::now();
            let removed = forget_one(memory, strategy).await?;
            let ns = elapsed_ns(start);
            anyhow::ensure!(removed, "profile forget must remove its fixture");
            restore()?; // Outside the measured operation; return corpus to N.
            if index != 0 {
                match strategy {
                    Strategy::LegacyPrefix => old.push(ns),
                    Strategy::Current => current.push(ns),
                }
            }
        }
    }
    Ok(json!({"case":"forget_existing_global_in_host_scope",
        "legacy_prefix_then_delete":sample_summary(&old),"current_forget_for_agent":sample_summary(&current),
        "restore_in_timing":false}))
}

async fn measure_current_new_write(memory: &TachiMemory) -> anyhow::Result<Value> {
    let mut samples = Vec::with_capacity(SAMPLES);
    let body = payload(50_001);
    for index in 0..=SAMPLES {
        let start = Instant::now();
        memory
            .store_with_agent(
                NEW_WRITE_KEY,
                &body,
                MemoryCategory::Core,
                None,
                None,
                None,
                Some(AGENT),
            )
            .await?;
        let ns = elapsed_ns(start);
        // Lookup/cleanup is outside timing and uses the same admitted owner.
        let row = memory
            .get_for_agent(NEW_WRITE_KEY, AGENT)
            .await?
            .context("profile new-write fixture not found")?;
        anyhow::ensure!(row.content == body, "profile stored payload mismatch");
        let removed = memory
            .store_handle()
            .lock()
            .delete(&row.id)
            .map_err(|error| anyhow::Error::msg(format!("profile cleanup: {error}")))?;
        anyhow::ensure!(removed, "profile new-write cleanup failed");
        if index != 0 {
            samples.push(ns);
        }
    }
    Ok(
        json!({"case":"new_core_write_current_only","current_store_with_agent":sample_summary(&samples),
        "cleanup_in_timing":false,"comparison_note":"No synthetic old-write timing; the paired new-key-miss case measures only its lookup cost."}),
    )
}

fn projected_bytes(rows: &[portable_kernel::PathPrefixMemoryRow]) -> usize {
    rows.iter()
        .map(|row| {
            row.id.len()
                + row.path.len()
                + row.category.len()
                + row.timestamp.len()
                + row.metadata.len()
        })
        .sum()
}

/// Untimed diagnostics perform actual typed queries. Byte totals count returned
/// text/metadata strings, not allocator overhead, peak RSS or SQLite rows scanned.
fn query_diagnostics(memory: &TachiMemory) -> anyhow::Result<Value> {
    let store = memory.store_handle().lock();
    let prefix = TachiMemory::agent_path_prefix(Some(AGENT));
    let mut page_limit = DEFAULT_LIST_PAGE_SIZE;
    let mut pages = Vec::new();
    loop {
        let rows = store
            .list_by_path(&prefix, page_limit, false)
            .map_err(|error| anyhow::Error::msg(format!("profile legacy diagnostic: {error}")))?;
        let text_bytes: usize = rows.iter().map(|row| row.text.len()).sum();
        let metadata_bytes = rows.iter().try_fold(0usize, |total, row| {
            serde_json::to_vec(&row.metadata).map(|bytes| total + bytes.len())
        })?;
        let count = rows.len();
        pages.push(json!({"requested_limit":page_limit,"returned_rows":count,"returned_text_bytes":text_bytes,"returned_metadata_bytes":metadata_bytes}));
        if count < page_limit {
            break;
        }
        anyhow::ensure!(
            page_limit < MAX_LIST_GROW,
            "profile legacy diagnostic exceeded complete-list cap"
        );
        page_limit = page_limit.saturating_mul(2).min(MAX_LIST_GROW);
    }
    let broad = store
        .list_memories_by_path_prefix(&format!("{prefix}%"))
        .map_err(|error| anyhow::Error::msg(format!("profile projected diagnostic: {error}")))?;
    let broad_stats =
        json!({"returned_headers":broad.len(),"returned_string_bytes":projected_bytes(&broad)});
    drop(broad);
    let mut keyed = Vec::new();
    for case in &CASES {
        let rows = TachiMemory::rows_matching_key(&store, case.key, Some(AGENT))?;
        let text_bytes: usize = rows.iter().map(|row| row.text.len()).sum();
        let metadata_bytes = rows.iter().try_fold(0usize, |total, row| {
            serde_json::to_vec(&row.metadata).map(|bytes| total + bytes.len())
        })?;
        keyed.push(json!({"case":case.name,"hydrated_full_rows":rows.len(),"hydrated_text_bytes":text_bytes,
            "hydrated_metadata_bytes":metadata_bytes}));
    }
    Ok(
        json!({"legacy_full_entry_pages":pages,"complete_metadata_projection_per_point_operation":broad_stats,
            "matching_key_body_hydration":keyed}),
    )
}

fn file_bytes(path: &Path) -> anyhow::Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn data_sizes(memory: &TachiMemory, totals: &CorpusTotals) -> anyhow::Result<Value> {
    Ok(
        json!({"rows":totals.rows,"input_text_bytes":totals.text_bytes,
        "input_summary_bytes":totals.summary_bytes,"input_metadata_bytes":totals.input_metadata_bytes,
        "input_vector_bytes":0,"db_file_bytes":file_bytes(&memory.db_path)?,
        "wal_file_bytes":file_bytes(&memory.db_path.with_file_name("tachi.db-wal"))?,
        "shm_file_bytes":file_bytes(&memory.db_path.with_file_name("tachi.db-shm"))?}),
    )
}

fn save_report(path: &Path, report: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "manual real-kernel 1k/10k/50k profile; set ZEROCLAW_TACHI_PROFILE_OUTPUT"]
async fn profile_1k_10k_50k() -> anyhow::Result<()> {
    let output = std::env::var_os("ZEROCLAW_TACHI_PROFILE_OUTPUT")
        .context("set ZEROCLAW_TACHI_PROFILE_OUTPUT to a scratch JSON path")?;
    let output = PathBuf::from(output);
    let mut report = json!({
        "status":"running", "started_at_utc":chrono::Utc::now().to_rfc3339(),
        "product_base":"74f865d900eee050f2586dd3a695d6086ec69df4",
        "legacy_algorithm_base":"0fc1e8fae36d05e0ce7efb010906ec1c7da1aa60",
        "portable_kernel_revision":"b0c754a0cd3e4e7ffd0199691e08954942b492bd",
        "profile_revision":std::env::var("ZEROCLAW_TACHI_PROFILE_REVISION").ok(),
        "toolchain":std::env::var("ZEROCLAW_TACHI_PROFILE_TOOLCHAIN").ok(),
        "platform":{"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"debug_assertions":cfg!(debug_assertions)},
        "method":{"corpus_rows":TARGETS,"body_bytes_per_row":BODY_BYTES,"embedding":"none / NoopEmbedding",
            "measured_samples":SAMPLES,"warmup_per_strategy":1,"order":"alternating paired old/current",
            "quantiles":"Empirical nearest-rank p95 of nine samples is the maximum; this is a small local profile, not a tail-SLO estimate.",
            "cache":"warm; OS caches are not flushed",
            "seed":"one typed import_snapshot_batch into a fresh admitted TachiMemory per corpus size; seed and readback verification excluded from operation samples",
            "seed_lifecycle":"synthetic created_at/updated_at fixed to 2026-01-01T00:00:00.000Z; no supersession, access history or vectors",
            "legacy":"retained production list_prefix_complete + pre-F09 logical-key filter + shared PointScope",
            "current":"actual public Memory get_for_agent, forget_for_agent, store_with_agent",
            "limits":"No O(1), throughput, heap/RSS, release-build or query-plan claim. Every point read/delete still scans O(N) metadata; repeated new keys remain worst-case O(N^2) metadata."},
        "stages":[]
    });
    save_report(&output, &report)?;
    report["snapshot_fixture_parity_preflight"] = verify_snapshot_fixture_parity()?;
    save_report(&output, &report)?;
    for target in TARGETS {
        let directory = tempfile::TempDir::new()?;
        let memory = TachiMemory::new("synthetic-profile", directory.path())?;
        anyhow::ensure!(
            memory.count().await? == 0,
            "profile seed target is not empty"
        );
        let seed_start = Instant::now();
        let (totals, seed_evidence) = seed_snapshot(&memory, target)?;
        let seed_ms = seed_start.elapsed().as_secs_f64() * 1000.0;
        anyhow::ensure!(
            memory.count().await? == target,
            "profile corpus count mismatch"
        );
        let mut stage = json!({"rows":target,"fresh_corpus_seed_and_verification_ms":seed_ms,
            "seed_evidence":seed_evidence,"data_before":data_sizes(&memory,&totals)?,
            "untimed_query_diagnostics":query_diagnostics(&memory)?,"operations":[]});
        for case in &CASES {
            let measurement = measure_lookup(&memory, case).await?;
            println!("TACHI_PROFILE rows={target} {}", measurement);
            stage["operations"]
                .as_array_mut()
                .unwrap()
                .push(measurement);
        }
        for measurement in [
            measure_forget(&memory).await?,
            measure_current_new_write(&memory).await?,
        ] {
            println!("TACHI_PROFILE rows={target} {}", measurement);
            stage["operations"]
                .as_array_mut()
                .unwrap()
                .push(measurement);
        }
        anyhow::ensure!(
            memory.count().await? == target,
            "profile mutation cleanup changed corpus size"
        );
        stage["data_after"] = data_sizes(&memory, &totals)?;
        report["stages"].as_array_mut().unwrap().push(stage);
        save_report(&output, &report)?;
    }
    report["status"] = "completed".into();
    report["completed_at_utc"] = chrono::Utc::now().to_rfc3339().into();
    save_report(&output, &report)?;
    println!("TACHI_PROFILE_REPORT {}", output.display());
    Ok(())
}
