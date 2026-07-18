//! Optional LLM enrichment for tachi / memcore raw memories (feature `tachi`).
//!
//! RomanBath `extract_facts` parity: summary + keywords + entities via a
//! per-call [`ModelProvider`]. No stored provider handle, no background thread.
//!
//! **Importance:** the enricher JSON may include `importance`, but memcore's
//! public `update_enrichment_fields` has no importance parameter and we do not
//! reach into SQL — so importance is deliberately not written (and not parsed).
//!
//! # Adjudicated non-fixes
//!
//! - **Global backfill via `AgentScopedMemory`:** enrichment intentionally walks
//!   the shared install store (one DB per install). The ≤16-row / 12h window
//!   makes cross-agent budget negligible; identity fields remain untouchable
//!   through `update_enrichment_fields`.
//! - **Gateway WS path:** `zeroclaw-gateway` calls `consolidation::consolidate_turn`
//!   directly (`ws.rs`) and therefore never triggers enrichment — known
//!   limitation. The orchestrator / `DefaultMemoryStrategy` path is the
//!   supported cadence.

use memcore::MemoryStore;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_providers::ProviderDispatch;

/// Max raw entries enriched in one pass (unattended batch bound).
const ENRICHMENT_BATCH_LIMIT: usize = 16;

/// Max keywords written per enrichment update (LLM output bound).
const MAX_KEYWORDS: usize = 16;

/// Max entities written per enrichment update (LLM output bound).
const MAX_ENTITIES: usize = 16;

/// Max UTF-8 chars kept per keyword / entity string after trim.
const MAX_TAG_CHARS: usize = 64;

const ENRICH_SYSTEM_PROMPT: &str = r#"You are a memory enrichment system for an autonomous agent.
Given a raw memory note, extract structured fields for recall.

Output JSON:
{
  "summary": "≤100 char summary",
  "keywords": ["tag1", "tag2"],
  "entities": ["entity1"],
  "importance": 0.0-1.0
}

Output ONLY valid JSON, no other text."#;

#[derive(Debug, serde::Deserialize)]
struct EnrichmentParse {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
}

struct EnrichCandidate {
    id: String,
    text: String,
    revision: i64,
    /// Row already has a non-empty summary — do not overwrite.
    has_summary: bool,
    /// Row already has keywords — do not overwrite.
    has_keywords: bool,
    /// Row already has entities — do not overwrite.
    has_entities: bool,
}

/// Run one enrichment pass on the live store handle.
///
/// LLM / parse failures skip the entry and continue (RomanBath contract).
/// Returns the number of successfully written enrichments.
pub async fn run_enrichment_pass(
    store: &Arc<Mutex<MemoryStore>>,
    provider: &dyn ModelProvider,
    model: &str,
) -> anyhow::Result<usize> {
    let candidates = collect_candidates(store)?;
    if candidates.is_empty() {
        return Ok(0);
    }

    let mut enriched = 0usize;
    for cand in candidates {
        let parsed = match request_enrichment(provider, model, &cand.text).await {
            Ok(p) => p,
            Err(_) => continue,
        };

        let summary_owned = truncate_summary(parsed.summary.trim());
        let keywords = bound_tags(parsed.keywords, MAX_KEYWORDS);
        let entities = bound_tags(parsed.entities, MAX_ENTITIES);

        // Only Some(...) for a field we are genuinely filling. Skip fields that
        // (a) already exist on the row, or (b) came back empty from the parse.
        let summary_arg: Option<&str> = if cand.has_summary || summary_owned.is_empty() {
            None
        } else {
            Some(summary_owned.as_str())
        };
        let keywords_arg: Option<&[String]> = if cand.has_keywords || keywords.is_empty() {
            None
        } else {
            Some(keywords.as_slice())
        };
        let entities_arg: Option<&[String]> = if cand.has_entities || entities.is_empty() {
            None
        } else {
            Some(entities.as_slice())
        };

        if summary_arg.is_none() && keywords_arg.is_none() && entities_arg.is_none() {
            continue;
        }

        let wrote = {
            let mut guard = store.lock();
            match guard.update_enrichment_fields(
                &cand.id,
                summary_arg,
                None, // embeddings stay on the existing embedder path
                keywords_arg,
                entities_arg,
                cand.revision,
            ) {
                Ok(true) => true,
                Ok(false) => false, // revision mismatch — skip
                Err(_) => false,
            }
        };
        if wrote {
            enriched += 1;
        }
    }
    Ok(enriched)
}

fn collect_candidates(store: &Arc<Mutex<MemoryStore>>) -> anyhow::Result<Vec<EnrichCandidate>> {
    let guard = store.lock();
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let missing_summaries = guard
        .entries_missing_summaries()
        .map_err(|e| anyhow::Error::msg(format!("tachi enrichment: missing summaries: {e}")))?;
    for (id, text, revision) in missing_summaries {
        push_raw_candidate(&guard, &mut seen, &mut out, id, text, revision)?;
        if out.len() >= ENRICHMENT_BATCH_LIMIT {
            return Ok(out);
        }
    }

    let missing_meta = guard
        .entries_missing_metadata()
        .map_err(|e| anyhow::Error::msg(format!("tachi enrichment: missing metadata: {e}")))?;
    for (id, text, _summary, revision) in missing_meta {
        push_raw_candidate(&guard, &mut seen, &mut out, id, text, revision)?;
        if out.len() >= ENRICHMENT_BATCH_LIMIT {
            return Ok(out);
        }
    }

    Ok(out)
}

fn push_raw_candidate(
    store: &MemoryStore,
    seen: &mut HashSet<String>,
    out: &mut Vec<EnrichCandidate>,
    id: String,
    text: String,
    revision: i64,
) -> anyhow::Result<()> {
    if !seen.insert(id.clone()) {
        return Ok(());
    }
    // memcore scanners (entries_missing_summaries / entries_missing_metadata at
    // rev 7ae2c0a0) have no anchor/tier/LIMIT filter — post-filtering is ours.
    if id.starts_with("anchor:") {
        return Ok(());
    }
    let Some(entry) = store
        .get(&id)
        .map_err(|e| anyhow::Error::msg(format!("tachi enrichment: get {id}: {e}")))?
    else {
        return Ok(());
    };
    if entry.archived || !entry.tier.eq_ignore_ascii_case("raw") {
        return Ok(());
    }
    // Already fully enriched (scanner race / non-empty summary+keywords).
    let has_summary = !entry.summary.trim().is_empty();
    let has_keywords = !entry.keywords.is_empty();
    let has_entities = !entry.entities.is_empty();
    if has_summary && has_keywords {
        return Ok(());
    }
    out.push(EnrichCandidate {
        id,
        text,
        revision,
        has_summary,
        has_keywords,
        has_entities,
    });
    Ok(())
}

async fn request_enrichment(
    provider: &dyn ModelProvider,
    model: &str,
    text: &str,
) -> anyhow::Result<EnrichmentParse> {
    let user_msg = format!("Memory note:\n{text}");
    let raw = ProviderDispatch::from_ref(provider)
        .chat_with_system(Some(ENRICH_SYSTEM_PROMPT), &user_msg, model, Some(0.3))
        .await?;
    parse_enrichment_response(&raw)
}

fn parse_enrichment_response(raw: &str) -> anyhow::Result<EnrichmentParse> {
    let cleaned = strip_json_fences(raw);
    let parsed: EnrichmentParse = serde_json::from_str(&cleaned)
        .map_err(|e| anyhow::Error::msg(format!("enrichment JSON parse failed: {e}")))?;
    Ok(parsed)
}

fn strip_json_fences(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let without_fence = without_fence
        .strip_suffix("```")
        .unwrap_or(without_fence)
        .trim();
    // Prefer innermost `{...}` if the model added prose.
    if let (Some(start), Some(end)) = (without_fence.find('{'), without_fence.rfind('}'))
        && start < end
    {
        return without_fence[start..=end].to_string();
    }
    without_fence.to_string()
}

fn truncate_summary(summary: &str) -> String {
    summary.chars().take(100).collect()
}

/// Trim, drop empties, cap per-string length and total count before write.
fn bound_tags(tags: Vec<String>, max_count: usize) -> Vec<String> {
    tags.into_iter()
        .filter_map(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(trimmed.chars().take(MAX_TAG_CHARS).collect::<String>())
        })
        .take(max_count)
        .collect()
}

#[cfg(all(test, feature = "tachi"))]
mod tests {
    use super::*;
    use crate::hygiene;
    use crate::tachi::TachiMemory;
    use crate::traits::Memory;
    use async_trait::async_trait;
    use memcore::MemoryEntry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
    use zeroclaw_api::model_provider::ModelProvider;

    struct FixedJsonProvider {
        body: String,
        calls: AtomicUsize,
        fail: bool,
    }

    impl FixedJsonProvider {
        fn ok(body: &str) -> Self {
            Self {
                body: body.into(),
                calls: AtomicUsize::new(0),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                body: String::new(),
                calls: AtomicUsize::new(0),
                fail: true,
            }
        }
    }

    impl Attributable for FixedJsonProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "FixedJsonProvider"
        }
    }

    #[async_trait]
    impl ModelProvider for FixedJsonProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("provider error");
            }
            Ok(self.body.clone())
        }
    }

    fn seed_raw(
        store: &mut MemoryStore,
        id: &str,
        text: &str,
        summary: &str,
        keywords: &[&str],
        tier: &str,
        revision: i64,
    ) {
        let now = chrono::Local::now().to_rfc3339();
        let entry = MemoryEntry {
            id: id.into(),
            path: format!("/agents/default/default/core/{id}"),
            summary: summary.into(),
            text: text.into(),
            importance: 0.5,
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
            revision,
            vector: None,
            retention_policy: None,
            domain: None,
            metadata: serde_json::json!({
                "zeroclaw_key": id,
                "zeroclaw_category": "core",
                "zeroclaw_namespace": "default",
                "zeroclaw_agent": "default",
            }),
            recall_count: 0,
            query_diversity: 0,
            tier: tier.into(),
        };
        store.upsert(&entry).expect("seed");
    }

    #[tokio::test]
    async fn enrich_writes_summary_keywords_entities() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-1",
                "Kyle prefers Asia/Shanghai timezone for trading reports",
                "",
                &[],
                "raw",
                1,
            );
        }

        let provider = FixedJsonProvider::ok(
            r#"{"summary":"Prefers Asia/Shanghai","keywords":["timezone","kyle"],"entities":["Asia/Shanghai"],"importance":0.9}"#,
        );
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let store = mem.store_handle().lock();
        let row = store.get("raw-1").unwrap().expect("row");
        assert_eq!(row.summary, "Prefers Asia/Shanghai");
        assert!(row.keywords.iter().any(|k| k == "timezone"));
        assert!(row.entities.iter().any(|e| e == "Asia/Shanghai"));
        // memcore update_enrichment_fields is revision-checked but does not
        // bump revision; success with expected_revision=1 proves the check.
        assert_eq!(row.revision, 1);
    }

    #[tokio::test]
    async fn enrich_preserves_existing_summary_when_llm_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-partial",
                "note that already has a summary but needs keywords",
                "Keep this summary",
                &[],
                "raw",
                1,
            );
        }

        let provider = FixedJsonProvider::ok(
            r#"{"summary":"","keywords":["timezone","trading"],"entities":[]}"#,
        );
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let store = mem.store_handle().lock();
        let row = store.get("raw-partial").unwrap().unwrap();
        assert_eq!(row.summary, "Keep this summary");
        assert!(row.keywords.iter().any(|k| k == "timezone"));
        assert!(row.keywords.iter().any(|k| k == "trading"));
    }

    #[tokio::test]
    async fn enrich_fills_missing_keywords_without_replacing_populated_fields() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-mixed",
                "note has a summary and entity but needs keywords",
                "Keep this summary",
                &[],
                "raw",
                1,
            );
            let mut row = store.get("raw-mixed").unwrap().unwrap();
            row.entities = vec!["Keep this entity".into()];
            store.upsert(&row).unwrap();
        }

        let provider = FixedJsonProvider::ok(
            r#"{"summary":"replace summary","keywords":["new-keyword"],"entities":["replace entity"]}"#,
        );
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 1);

        let store = mem.store_handle().lock();
        let row = store.get("raw-mixed").unwrap().unwrap();
        assert_eq!(row.summary, "Keep this summary");
        assert_eq!(row.entities, vec!["Keep this entity"]);
        assert_eq!(row.keywords, vec!["new-keyword"]);
    }

    #[tokio::test]
    async fn enrich_bounds_keywords_and_entities_before_write() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-bound",
                "note that needs bounded tags",
                "",
                &[],
                "raw",
                1,
            );
        }

        let long = "x".repeat(500);
        let keywords: Vec<String> = (0..100).map(|i| format!("{long}-{i}")).collect();
        let entities: Vec<String> = (0..100).map(|i| format!("ent-{long}-{i}")).collect();
        let body = serde_json::json!({
            "summary": "Bounded tags",
            "keywords": keywords,
            "entities": entities,
        });
        let provider = FixedJsonProvider::ok(&body.to_string());
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 1);

        let store = mem.store_handle().lock();
        let row = store.get("raw-bound").unwrap().unwrap();
        assert!(row.keywords.len() <= MAX_KEYWORDS);
        assert!(row.entities.len() <= MAX_ENTITIES);
        for k in &row.keywords {
            assert!(k.chars().count() <= MAX_TAG_CHARS);
        }
        for e in &row.entities {
            assert!(e.chars().count() <= MAX_TAG_CHARS);
        }
    }

    #[tokio::test]
    async fn enrich_skips_anchor_ids() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        let anchor_id = {
            let store = mem.store_handle().lock();
            // Anchors have empty keywords → entries_missing_metadata surfaces
            // them; post-filter must skip by id prefix (and non-raw tier).
            store
                .ensure_anchor(memcore::AnchorKind::Issue, "enrich-skip")
                .unwrap()
        };
        assert!(anchor_id.starts_with("anchor:"));

        let provider = FixedJsonProvider::ok(
            r#"{"summary":"should not apply","keywords":["x"],"entities":[]}"#,
        );
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

        let store = mem.store_handle().lock();
        let row = store.get(&anchor_id).unwrap().unwrap();
        assert!(row.keywords.is_empty());
        assert_eq!(row.revision, 1);
    }

    #[tokio::test]
    async fn enrich_parses_markdown_fenced_json() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-fence",
                "fenced json response note",
                "",
                &[],
                "raw",
                1,
            );
        }

        let provider = FixedJsonProvider::ok(
            "```json\n{\"summary\":\"From fence\",\"keywords\":[\"fenced\"],\"entities\":[]}\n```",
        );
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 1);

        let store = mem.store_handle().lock();
        let row = store.get("raw-fence").unwrap().unwrap();
        assert_eq!(row.summary, "From fence");
        assert!(row.keywords.iter().any(|k| k == "fenced"));
    }

    #[tokio::test]
    async fn enrich_skips_wrong_type_keywords_leaves_row_unchanged() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-wrong-type",
                "wrong keywords type note",
                "",
                &[],
                "raw",
                1,
            );
        }

        let provider =
            FixedJsonProvider::ok(r#"{"summary":"should not write","keywords":"not-an-array"}"#);
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let store = mem.store_handle().lock();
        let row = store.get("raw-wrong-type").unwrap().unwrap();
        assert_eq!(row.summary, "");
        assert!(row.keywords.is_empty());
        assert_eq!(row.revision, 1);
    }

    #[tokio::test]
    async fn enrich_skips_provider_errors_and_garbage() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-bad",
                "unchanged note body",
                "",
                &[],
                "raw",
                1,
            );
        }

        let failing = FixedJsonProvider::failing();
        let n = mem
            .run_llm_enrichment(&failing, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 0);

        let garbage = FixedJsonProvider::ok("not-json-at-all");
        let n = mem
            .run_llm_enrichment(&garbage, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 0);

        let store = mem.store_handle().lock();
        let row = store.get("raw-bad").unwrap().unwrap();
        assert_eq!(row.summary, "");
        assert!(row.keywords.is_empty());
        assert_eq!(row.revision, 1);
    }

    #[tokio::test]
    async fn enrich_skips_non_raw_and_already_enriched() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "cons-1",
                "consolidated twin text",
                "",
                &[],
                "consolidated",
                1,
            );
            seed_raw(
                &mut store,
                "raw-done",
                "already enriched raw note",
                "Existing summary",
                &["kept"],
                "raw",
                1,
            );
        }

        let provider = FixedJsonProvider::ok(
            r#"{"summary":"should not apply","keywords":["x"],"entities":[],"importance":0.5}"#,
        );
        let n = mem
            .run_llm_enrichment(&provider, "test-model")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

        let store = mem.store_handle().lock();
        assert_eq!(store.get("cons-1").unwrap().unwrap().summary, "");
        assert_eq!(
            store.get("raw-done").unwrap().unwrap().summary,
            "Existing summary"
        );
    }

    #[tokio::test]
    async fn enrichment_cadence_gates_via_state_file() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-c",
                "cadence candidate note",
                "",
                &[],
                "raw",
                1,
            );
        }
        let provider = FixedJsonProvider::ok(
            r#"{"summary":"Cadence hit","keywords":["c"],"entities":[],"importance":0.5}"#,
        );

        assert!(hygiene::enrichment_is_due(tmp.path()).unwrap());
        let n = crate::run_llm_enrichment_if_due(&mem, tmp.path(), &provider, "m")
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(!hygiene::enrichment_is_due(tmp.path()).unwrap());

        // Not due → no further provider calls / writes.
        let calls_before = provider.calls.load(Ordering::SeqCst);
        let n = crate::run_llm_enrichment_if_due(&mem, tmp.path(), &provider, "m")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), calls_before);
    }

    #[tokio::test]
    async fn non_tachi_backend_enrichment_is_noop() {
        let provider = FixedJsonProvider::ok("{}");
        let n = crate::none::NoneMemory::new("none")
            .run_llm_enrichment(&provider, "m")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn revision_mismatch_skips_write() {
        let tmp = TempDir::new().unwrap();
        let mem = TachiMemory::new("tachi", tmp.path()).unwrap();
        {
            let mut store = mem.store_handle().lock();
            seed_raw(
                &mut store,
                "raw-rev",
                "revision race note",
                "",
                &[],
                "raw",
                1,
            );
            // Bump revision behind the candidate snapshot by rewriting the row.
            let mut row = store.get("raw-rev").unwrap().unwrap();
            row.revision = 2;
            row.text = "revision race note (edited)".into();
            store.upsert(&row).unwrap();
        }
        // Candidate collection sees revision 2; we force a stale expected
        // revision by calling update directly after collecting at rev 2 then
        // bumping again before write — simulate via update_enrichment_fields.
        {
            let mut store = mem.store_handle().lock();
            let mut row = store.get("raw-rev").unwrap().unwrap();
            row.revision = 3;
            store.upsert(&row).unwrap();
            let ok = store
                .update_enrichment_fields(
                    "raw-rev",
                    Some("stale"),
                    None,
                    Some(&["k".into()]),
                    Some(&[]),
                    2, // stale
                )
                .unwrap();
            assert!(!ok, "stale revision must not write");
            let row = store.get("raw-rev").unwrap().unwrap();
            assert_ne!(row.summary, "stale");
        }
    }

    #[test]
    fn parse_enrichment_response_accepts_fenced_json() {
        let raw = "```json\n{\"summary\":\"Hi\",\"keywords\":[\"a\"],\"entities\":[]}\n```";
        let parsed = parse_enrichment_response(raw).unwrap();
        assert_eq!(parsed.summary, "Hi");
        assert_eq!(parsed.keywords, vec!["a".to_string()]);
    }

    #[test]
    fn parse_enrichment_response_rejects_keywords_wrong_type() {
        let raw = r#"{"summary":"x","keywords":"not-an-array"}"#;
        assert!(parse_enrichment_response(raw).is_err());
    }
}
