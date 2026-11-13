//! Translate existing host scope into the kernel's pre-limit read predicates.

use super::{
    DEFAULT_AGENT, DEFAULT_NAMESPACE, METADATA_SESSION_ID, METADATA_ZC_AGENT, METADATA_ZC_NAMESPACE,
};
use portable_kernel::store::scoped_read::{MetadataTextFilter, RecentMemoryQuery, TextMembership};
use portable_kernel::{MemoryEntry, MemoryStore};
use std::collections::HashSet;

// The pinned kernel bounds metadata values per query, not the host allowlist.
const MAX_QUERY_VALUES: usize = 256;

pub(super) fn queries(
    prefix: String,
    limit: usize,
    session_id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    namespace: Option<&str>,
    allowed_agents: Option<&[&str]>,
) -> Vec<RecentMemoryQuery> {
    let mut query = RecentMemoryQuery::new(prefix, limit);
    // Match entry_in_window's literal comparisons, without normalizing bounds.
    query.since = since.map(str::to_owned);
    query.until = until.map(str::to_owned);
    if let Some(session) = session_id {
        query.filters.push(MetadataTextFilter {
            field: METADATA_SESSION_ID.into(),
            values: vec![session.into()],
            membership: TextMembership::AnyOf,
            missing_value: None,
        });
    }
    let (membership, value) = match namespace {
        Some(namespace) => (TextMembership::AnyOf, namespace),
        None => (TextMembership::NoneOf, crate::SOUL_NAMESPACE),
    };
    query.filters.push(MetadataTextFilter {
        field: METADATA_ZC_NAMESPACE.into(),
        values: vec![value.into()],
        membership,
        missing_value: Some(DEFAULT_NAMESPACE.into()),
    });
    let Some(allowed) = allowed_agents else {
        return vec![query];
    };
    let fixed_values: usize = query.filters.iter().map(|filter| filter.values.len()).sum();
    allowed
        .chunks(MAX_QUERY_VALUES - fixed_values)
        .map(|agents| {
            let mut batch = query.clone();
            batch.filters.push(MetadataTextFilter {
                field: METADATA_ZC_AGENT.into(),
                values: agents.iter().map(|agent| (*agent).to_owned()).collect(),
                membership: TextMembership::AnyOf,
                missing_value: Some(DEFAULT_AGENT.into()),
            });
            batch
        })
        .collect()
}

pub(super) fn read(
    store: &MemoryStore,
    queries: &[RecentMemoryQuery],
    limit: usize,
) -> anyhow::Result<Vec<MemoryEntry>> {
    let mut rows: Vec<MemoryEntry> = Vec::new();
    for query in queries {
        rows.extend(
            store
                .list_recent_matching(query)
                .map_err(|e| anyhow::Error::msg(format!("tachi list_recent failed: {e}")))?,
        );
        // Each batch has the same predicates and limit. Its top `limit` rows
        // suffice for the global top `limit`, ordered exactly as the kernel.
        rows.sort_unstable_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| a.id.cmp(&b.id)));
        let mut seen = HashSet::new();
        rows.retain(|row| seen.insert(row.id.clone()));
        rows.truncate(limit);
    }
    Ok(rows)
}
