//! Tachi / memcore light-sleep governance (feature `tachi`).
//!
//! Driven by [`crate::run_tachi_governance`] from
//! `DefaultMemoryStrategy::run_governance` when `memory.backend` is `tachi`.
//! Uses memcore public APIs only — no direct SQL into the kernel schema.

use anyhow::Context;
use memcore::{MemoryEntry, MemoryStore, NEAR_DUP_RAW_SCAN_CAP, near_duplicate_raw_pairs};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use zeroclaw_config::schema::MemoryConfig;

use crate::backend_kind_from_dotted;

/// Report counters from one governance pass (observability only — not persisted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TachiGovernanceReport {
    pub near_dup_archived: usize,
    pub near_dup_survivors_updated: usize,
    pub promoted: usize,
    pub stale_archived: usize,
}

/// Run tachi governance when the active memory backend is `tachi`.
///
/// No-op for other backends. Intended to be called from
/// [`zeroclaw_runtime::agent::memory_strategy::DefaultMemoryStrategy::run_governance`].
pub fn run_tachi_governance(
    config: &MemoryConfig,
    workspace_dir: &Path,
) -> anyhow::Result<TachiGovernanceReport> {
    if backend_kind_from_dotted(&config.backend) != "tachi" {
        return Ok(TachiGovernanceReport::default());
    }
    run_on_workspace(config, workspace_dir)
}

fn tachi_db_path(workspace_dir: &Path) -> std::path::PathBuf {
    workspace_dir.join("memory").join("tachi.db")
}

fn run_on_workspace(
    config: &MemoryConfig,
    workspace_dir: &Path,
) -> anyhow::Result<TachiGovernanceReport> {
    let db_path = tachi_db_path(workspace_dir);
    if !db_path.exists() {
        return Ok(TachiGovernanceReport::default());
    }
    let mut store = MemoryStore::open(
        db_path
            .to_str()
            .context("tachi memory db path is not valid UTF-8")?,
    )
    .map_err(|e| anyhow::Error::msg(format!("tachi governance: open db failed: {e}")))?;

    let mut report = TachiGovernanceReport::default();
    let near = run_near_dup_light_sleep(&mut store, config.dedup_jaccard_threshold)?;
    report.near_dup_archived = near.0;
    report.near_dup_survivors_updated = near.1;

    report.promoted = store
        .promote_diversely_recalled_raw_memories()
        .map_err(|e| anyhow::Error::msg(format!("tachi governance: promote failed: {e}")))?;

    report.stale_archived = store
        .archive_stale_low_value_memories()
        .map_err(|e| anyhow::Error::msg(format!("tachi governance: stale archive failed: {e}")))?;

    Ok(report)
}

/// Near-dup light-sleep: collapse connected components of raw-tier text twins
/// into one survivor (higher importance, then newer), merge keywords, archive
/// sources. Mirrors Sigil consolidate star-collapse ordering.
fn run_near_dup_light_sleep(
    store: &mut MemoryStore,
    threshold: f64,
) -> anyhow::Result<(usize, usize)> {
    let scan_limit = NEAR_DUP_RAW_SCAN_CAP.max(64);
    let entries = store
        .list_by_path("/agents", scan_limit, false)
        .map_err(|e| anyhow::Error::msg(format!("tachi governance: list failed: {e}")))?;
    if entries.len() < 2 {
        return Ok((0, 0));
    }

    let pairs = near_duplicate_raw_pairs(&entries, threshold);
    if pairs.is_empty() {
        return Ok((0, 0));
    }

    let mut parent: Vec<usize> = (0..entries.len()).collect();
    let find = |parent: &mut [usize], mut x: usize| -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    };
    for (left, right, _) in &pairs {
        let root_left = find(&mut parent, *left);
        let root_right = find(&mut parent, *right);
        if root_left != root_right {
            parent[root_right] = root_left;
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for index in 0..entries.len() {
        let root = find(&mut parent, index);
        components.entry(root).or_default().push(index);
    }

    let mut archived = 0usize;
    let mut survivors_updated = 0usize;
    for mut members in components.into_values() {
        if members.len() < 2 {
            continue;
        }
        // Only raw members participate (pairs already filter raw, but a
        // component can only contain indices that appeared in an edge).
        members.retain(|&idx| entries[idx].tier.eq_ignore_ascii_case("raw"));
        if members.len() < 2 {
            continue;
        }
        let survivor_idx = pick_survivor_index(&entries, &members);
        let mut survivor = entries[survivor_idx].clone();
        let mut keyword_set: HashSet<String> = survivor.keywords.iter().cloned().collect();
        let mut source_ids = Vec::new();
        for &idx in &members {
            if idx == survivor_idx {
                continue;
            }
            for kw in &entries[idx].keywords {
                keyword_set.insert(kw.clone());
            }
            source_ids.push(entries[idx].id.clone());
        }
        let mut merged_keywords: Vec<String> = keyword_set.into_iter().collect();
        merged_keywords.sort();
        if merged_keywords != survivor.keywords {
            survivor.keywords = merged_keywords;
            store.upsert(&survivor).map_err(|e| {
                anyhow::Error::msg(format!("tachi governance: survivor upsert: {e}"))
            })?;
            survivors_updated += 1;
        }
        for id in source_ids {
            if store
                .archive_memory(&id)
                .map_err(|e| anyhow::Error::msg(format!("tachi governance: archive: {e}")))?
            {
                archived += 1;
            }
        }
    }
    Ok((archived, survivors_updated))
}

fn pick_survivor_index(entries: &[MemoryEntry], members: &[usize]) -> usize {
    let mut best = members[0];
    for &candidate in members.iter().skip(1) {
        if is_better_survivor(&entries[candidate], &entries[best]) {
            best = candidate;
        }
    }
    best
}

/// Higher importance wins; on tie prefer newer timestamp, then higher id.
fn is_better_survivor(a: &MemoryEntry, b: &MemoryEntry) -> bool {
    match a.importance.partial_cmp(&b.importance) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) => false,
        _ => match a.timestamp.cmp(&b.timestamp) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => a.id > b.id,
        },
    }
}

#[cfg(all(test, feature = "tachi"))]
mod tests {
    use super::*;
    use crate::tachi::TachiMemory;
    use crate::traits::{Memory, MemoryCategory};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn tachi_config() -> MemoryConfig {
        MemoryConfig {
            backend: "tachi".into(),
            dedup_jaccard_threshold: 0.9,
            ..MemoryConfig::default()
        }
    }

    fn seed_raw(
        store: &mut MemoryStore,
        id: &str,
        path: &str,
        text: &str,
        importance: f64,
        keywords: &[&str],
        tier: &str,
    ) {
        let now = chrono::Local::now().to_rfc3339();
        let entry = MemoryEntry {
            id: id.into(),
            path: path.into(),
            summary: text.chars().take(80).collect(),
            text: text.into(),
            importance,
            timestamp: now.clone(),
            valid_from: now,
            valid_until: None,
            category: "fact".into(),
            topic: String::new(),
            keywords: keywords.iter().map(|s| (*s).to_string()).collect(),
            persons: Vec::new(),
            entities: Vec::new(),
            location: String::new(),
            source: "zeroclaw-test".into(),
            scope: "general".into(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            vector: None,
            retention_policy: None,
            domain: None,
            metadata: serde_json::json!({}),
            recall_count: 0,
            query_diversity: 0,
            tier: tier.into(),
        };
        store.upsert(&entry).expect("seed upsert");
    }

    #[test]
    fn near_dup_merge_collapses_transitive_raw_chain_via_strategy_entry() {
        let tmp = TempDir::new().unwrap();
        // Ensure schema exists via TachiMemory open.
        let _mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        let db = tachi_db_path(tmp.path());
        let mut store = MemoryStore::open(db.to_str().unwrap()).unwrap();

        let base = "The operator prefers concise Rust answers for equity trading notes";
        seed_raw(
            &mut store,
            "raw-a",
            "/agents/default/ns/core/a",
            base,
            0.5,
            &["alpha"],
            "raw",
        );
        seed_raw(
            &mut store,
            "raw-b",
            "/agents/default/ns/core/b",
            &format!("{base} today"),
            0.9,
            &["bravo"],
            "raw",
        );
        seed_raw(
            &mut store,
            "raw-c",
            "/agents/default/ns/core/c",
            &format!("{base} always"),
            0.6,
            &["charlie"],
            "raw",
        );
        drop(store);

        // Strategy entry point — not near_dup directly.
        let report = run_tachi_governance(&tachi_config(), tmp.path()).unwrap();
        assert_eq!(
            report.near_dup_archived, 2,
            "two sources archived: {report:?}"
        );

        let store = MemoryStore::open(db.to_str().unwrap()).unwrap();
        let survivor = store.get("raw-b").unwrap().expect("survivor");
        assert!(!survivor.archived);
        assert!(survivor.keywords.iter().any(|k| k == "alpha"));
        assert!(survivor.keywords.iter().any(|k| k == "bravo"));
        assert!(survivor.keywords.iter().any(|k| k == "charlie"));
        assert!(
            store
                .get_with_options("raw-a", true)
                .unwrap()
                .unwrap()
                .archived
        );
        assert!(
            store
                .get_with_options("raw-c", true)
                .unwrap()
                .unwrap()
                .archived
        );
    }

    #[test]
    fn near_dup_leaves_non_raw_tiers_untouched() {
        let tmp = TempDir::new().unwrap();
        let _mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        let db = tachi_db_path(tmp.path());
        let mut store = MemoryStore::open(db.to_str().unwrap()).unwrap();

        let text = "Shared consolidated preference text about Rust tooling";
        seed_raw(
            &mut store,
            "cons-a",
            "/agents/default/ns/core/cons_a",
            text,
            0.9,
            &[],
            "consolidated",
        );
        seed_raw(
            &mut store,
            "cons-b",
            "/agents/default/ns/core/cons_b",
            &format!("{text} extra"),
            0.5,
            &[],
            "consolidated",
        );
        seed_raw(
            &mut store,
            "raw-twin",
            "/agents/default/ns/core/raw_twin",
            text,
            0.4,
            &[],
            "raw",
        );
        drop(store);

        let report = run_tachi_governance(&tachi_config(), tmp.path()).unwrap();
        assert_eq!(
            report.near_dup_archived, 0,
            "consolidated twins must not archive via near-dup: {report:?}"
        );

        let store = MemoryStore::open(db.to_str().unwrap()).unwrap();
        assert!(!store.get("cons-a").unwrap().expect("cons-a").archived);
        assert!(!store.get("cons-b").unwrap().expect("cons-b").archived);
        assert!(
            !store
                .get("raw-twin")
                .unwrap()
                .expect("raw-twin alone has no raw pair")
                .archived
        );
    }

    #[test]
    fn promote_and_stale_archive_smoke() {
        let tmp = TempDir::new().unwrap();
        let _mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        let db = tachi_db_path(tmp.path());
        let mut store = MemoryStore::open(db.to_str().unwrap()).unwrap();

        let now = chrono::Local::now().to_rfc3339();
        let promotable = MemoryEntry {
            id: "promo".into(),
            path: "/agents/default/ns/core/promo".into(),
            summary: "promotable".into(),
            text: "Diversely recalled raw note for promotion smoke".into(),
            importance: 0.8,
            timestamp: now.clone(),
            valid_from: now.clone(),
            valid_until: None,
            category: "fact".into(),
            topic: String::new(),
            keywords: Vec::new(),
            persons: Vec::new(),
            entities: Vec::new(),
            location: String::new(),
            source: "zeroclaw-test".into(),
            scope: "general".into(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            vector: None,
            retention_policy: None,
            domain: None,
            metadata: serde_json::json!({}),
            recall_count: 3,
            query_diversity: 3,
            tier: "raw".into(),
        };
        store.upsert(&promotable).unwrap();

        let mut stale = promotable.clone();
        stale.id = "stale".into();
        stale.path = "/agents/default/ns/core/stale".into();
        stale.text = "Old unused low-value raw note for stale archive".into();
        stale.importance = 0.2;
        stale.access_count = 0;
        stale.recall_count = 0;
        stale.query_diversity = 0;
        store.upsert(&stale).unwrap();
        store
            .connection()
            .execute(
                "UPDATE memories SET created_at = '2020-01-01T00:00:00Z' WHERE id = 'stale'",
                [],
            )
            .unwrap();
        drop(store);

        let report = run_tachi_governance(&tachi_config(), tmp.path()).unwrap();
        assert_eq!(report.promoted, 1, "expected one promotion: {report:?}");
        assert_eq!(
            report.stale_archived, 1,
            "expected one stale archive: {report:?}"
        );

        let store = MemoryStore::open(db.to_str().unwrap()).unwrap();
        assert_eq!(store.get("promo").unwrap().unwrap().tier, "consolidated");
        assert!(
            store
                .get_with_options("stale", true)
                .unwrap()
                .unwrap()
                .archived
        );
    }

    #[tokio::test]
    async fn strategy_noop_for_non_tachi_backend() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::with_embedder(
            "tachi",
            tmp.path(),
            Arc::new(crate::embeddings::NoopEmbedding),
            0.7,
            0.3,
        )
        .unwrap();
        mem.store("k", "content", MemoryCategory::Core, None)
            .await
            .unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            ..MemoryConfig::default()
        };
        let report = run_tachi_governance(&cfg, tmp.path()).unwrap();
        assert_eq!(report, TachiGovernanceReport::default());
    }
}
