//! Memory JSON-RPC method handlers extracted from dispatch.rs.

use super::dispatch::{RpcDispatcher, RpcResult, parse_params, rpc_err, to_result};
use super::types::*;
use serde_json::Value;
use zeroclaw_api::jsonrpc::error_codes::*;

impl RpcDispatcher {
    // ── Memory handlers ──────────────────────────────────────────

    pub(crate) async fn handle_memory_list(&self, params: &Value) -> RpcResult {
        let mem = self
            .ctx
            .memory
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Memory subsystem is not available"))?;
        let req: MemoryListParams = parse_params(params)?;
        let category = req
            .category
            .as_deref()
            .map(|s| MemoryCategory::Custom(s.to_string()));
        let entries = mem
            .list(category.as_ref(), req.session_id.as_deref())
            .await
            .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Memory list failed: {e}")))?;
        let count = entries.len();
        let entries = truncate_memory_previews(entries);
        to_result(MemoryListResult { entries, count })
    }

    pub(crate) async fn handle_memory_search(&self, params: &Value) -> RpcResult {
        let mem = self
            .ctx
            .memory
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Memory subsystem is not available"))?;
        let req: MemorySearchParams = parse_params(params)?;
        let entries = mem
            .recall(
                &req.query,
                req.limit,
                req.session_id.as_deref(),
                req.since.as_deref(),
                req.until.as_deref(),
            )
            .await
            .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Memory search failed: {e}")))?;
        let count = entries.len();
        let entries = truncate_memory_previews(entries);
        to_result(MemorySearchResult { entries, count })
    }

    /// `memory/get { key } → MemoryEntry`. Returns the full memory
    /// entry for one key so the Memory pane can keep only preview
    /// rows in memory and fetch the full `content` only when the
    /// detail pane opens. Dropped on detail close.
    pub(crate) async fn handle_memory_get(&self, params: &Value) -> RpcResult {
        let mem = self
            .ctx
            .memory
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Memory subsystem is not available"))?;
        let req: MemoryGetParams = parse_params(params)?;
        let entry = mem
            .get(&req.key)
            .await
            .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Memory get failed: {e}")))?;
        match entry {
            Some(e) => to_result(MemoryGetResult { entry: Some(e) }),
            None => Err(rpc_err(
                INTERNAL_ERROR,
                format!("Memory key `{}` not found", req.key),
            )),
        }
    }

    pub(crate) async fn handle_memory_store(&self, params: &Value) -> RpcResult {
        let mem = self
            .ctx
            .memory
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Memory subsystem is not available"))?;
        let req: MemoryStoreParams = parse_params(params)?;
        let category = req
            .category
            .as_deref()
            .map(|s| MemoryCategory::Custom(s.to_string()))
            .unwrap_or(MemoryCategory::Custom("user".into()));
        mem.store(&req.key, &req.content, category, req.session_id.as_deref())
            .await
            .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Memory store failed: {e}")))?;
        to_result(MemoryStoreResult {
            key: req.key,
            stored: true,
        })
    }

    pub(crate) async fn handle_memory_delete(&self, params: &Value) -> RpcResult {
        let mem = self
            .ctx
            .memory
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Memory subsystem is not available"))?;
        let req: MemoryDeleteParams = parse_params(params)?;
        mem.forget(&req.key)
            .await
            .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Memory delete failed: {e}")))?;
        to_result(MemoryDeleteResult {
            key: req.key,
            deleted: true,
        })
    }
}

const MEMORY_PREVIEW_CONTENT_BYTES: usize = 200;

/// Truncate each entry's `content` to the preview budget. Operates
/// in place to avoid a second allocation per entry.
fn truncate_memory_previews(
    mut entries: Vec<zeroclaw_api::memory_traits::MemoryEntry>,
) -> Vec<zeroclaw_api::memory_traits::MemoryEntry> {
    for entry in &mut entries {
        if entry.content.len() > MEMORY_PREVIEW_CONTENT_BYTES {
            // Truncate on a char boundary so we never split a UTF-8 sequence.
            let mut end = MEMORY_PREVIEW_CONTENT_BYTES;
            while end > 0 && !entry.content.is_char_boundary(end) {
                end -= 1;
            }
            entry.content.truncate(end);
            entry.content.push('…');
        }
    }
    entries
}
