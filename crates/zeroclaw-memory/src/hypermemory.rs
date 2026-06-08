//! HyperMemory backend — routes memory operations to the HyperMemory MCP service
//! via hapi-edge gateway (HTTP JSON-RPC).
//!
//! Contract: all memory operations are proxied through the MCP endpoint at
//! `http://127.0.0.1:6888/mcp` (or `HAPI_MEMORY_MCP_URL` env). The gateway
//! forwards to the HyperMemory MCP service which persists to its own storage.
//! Never access tachi MCP directly — route through hapi-edge only.
//!
//! ## MCP Tool Contract
//!
//! | MCP Tool       | Parameters                                                      |
//! |----------------|-----------------------------------------------------------------|
//! | `save_memory`  | `id`, `text`, `path`, `scope`, `project`, `domain`, `category`, `importance` |
//! | `search_memory`| `query`, `top_k`, `project`                       |
//! | `get_memory`   | `id` (= key), `project`                           |
//! | `delete_memory`| `id` (= key), `project`                           |
//! | `list_memories`| `project`                                         |

use crate::traits::{Memory, MemoryCategory, MemoryEntry};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Default MCP endpoint for the hapi-edge HyperMemory bridge.
const DEFAULT_MCP_URL: &str = "http://127.0.0.1:6888/mcp";

/// JSON-RPC request ID counter (monotonic).
static RPC_ID: AtomicU64 = AtomicU64::new(1);

/// Resolve the MCP endpoint URL from env or default.
fn mcp_url() -> String {
    std::env::var("HAPI_MEMORY_MCP_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MCP_URL.to_string())
}

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    params: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: Option<u64>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// HyperMemory backend that routes through hapi-edge MCP gateway.
pub struct HyperMemory {
    name: &'static str,
    client: reqwest::Client,
    namespace: String,
    /// Optional override for the MCP URL. When `None`, the URL is resolved
    /// from `HAPI_MEMORY_MCP_URL` env or the default. Used by tests to point
    /// at a `wiremock` server without touching the process environment.
    #[cfg(test)]
    url_override: Option<String>,
}

impl HyperMemory {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            client: reqwest::Client::new(),
            namespace: "hyperion".to_string(),
            #[cfg(test)]
            url_override: None,
        }
    }

    /// Create a HyperMemory instance pointing at a custom MCP URL.
    /// Used for testing against mock servers.
    #[cfg(test)]
    fn with_url(name: &'static str, url: &str) -> Self {
        Self {
            name,
            client: reqwest::Client::new(),
            namespace: "hyperion".to_string(),
            url_override: Some(url.to_string()),
        }
    }

    /// Send a JSON-RPC tool call to the MCP endpoint.
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let rpc_id = RPC_ID.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: rpc_id,
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": tool_name,
                "arguments": arguments,
            }),
        };

        let url = {
            #[cfg(test)]
            {
                self.url_override.clone().unwrap_or_else(mcp_url)
            }
            #[cfg(not(test))]
            {
                mcp_url()
            }
        };
        let response = self
            .client
            .post(&url)
            .json(&request)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"tool": tool_name, "error": e.to_string()})
                        ),
                    "HyperMemory MCP request failed"
                );
                anyhow::Error::msg(format!("HyperMemory MCP request failed: {e}"))
            })?;

        let rpc_response: JsonRpcResponse = response.json().await.map_err(|e| {
            anyhow::Error::msg(format!("HyperMemory MCP response parse error: {e}"))
        })?;

        if let Some(err) = rpc_response.error {
            anyhow::bail!("HyperMemory MCP error (code {}): {}", err.code, err.message);
        }

        Ok(rpc_response.result.unwrap_or(serde_json::Value::Null))
    }

    /// Extract text content from MCP tool result (standard MCP content array).
    fn extract_text_from_result(result: &serde_json::Value) -> String {
        // MCP tools return { content: [{ type: "text", text: "..." }] }
        if let Some(content_arr) = result.get("content").and_then(|c| c.as_array()) {
            let texts: Vec<&str> = content_arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect();
            return texts.join("\n");
        }
        // Fallback: return raw string if result is a simple string
        result.as_str().unwrap_or("").to_string()
    }

    /// Build the path identifier used by `save_memory`.
    /// Contract per AGENTS.md: path prefix is `/trading/equity/{key}`.
    fn build_memory_path(&self, key: &str) -> String {
        format!("/trading/equity/{key}")
    }

    /// Parse a single memory entry from a raw MCP JSON value.
    ///
    /// Real MCP responses return slim results (`id`, `path`, `text`, `created_at`)
    /// without `metadata.agent_id`. We therefore extract `agent_id` **exclusively**
    /// from the `id` field by splitting on `#`:
    ///
    ///   `id = "bias#agent-42"` → key `"bias"`, agent_id `Some("agent-42")`
    ///   `id = "bias"`           → key `"bias"`, agent_id `None`
    ///
    /// This guarantees the physical primary key is the sole authority for agent
    /// attribution, preventing metadata-less responses from being treated as legacy
    /// (unscoped) entries and leaking across agent boundaries.
    fn parse_memory_entry(
        &self,
        raw: &serde_json::Value,
        fallback_key: &str,
    ) -> Option<MemoryEntry> {
        // If the value is a string (bare text), wrap it.
        let raw = match raw.as_str() {
            Some(s) => serde_json::json!({ "text": s }),
            None => raw.clone(),
        };

        // Try direct deserialization first (works when the MCP response
        // already uses our MemoryEntry schema). However, we MUST still
        // enforce the `#` physical splitting on the deserialized entry's
        // `id` field. Without this, a remote returning a MemoryEntry-shaped
        // payload with `agent_id: None` would bypass the `#` splitting and
        // be treated as public memory, creating a confidentiality leak.
        if let Ok(mut entry) = serde_json::from_value::<MemoryEntry>(raw.clone()) {
            // Force `#` splitting on `entry.id` — the physical primary key
            // is the single source of truth for agent attribution.
            if let Some((k, agent)) = entry.id.split_once('#') {
                entry.key = k.to_string();
                entry.agent_id = Some(agent.to_string());
            } else {
                // No `#` means the physical key is unscoped. Do not trust a
                // deserialized `agent_id` field because the physical `id` is
                // the single source of truth for agent attribution.
                entry.agent_id = None;
            }
            return Some(entry);
        }

        // Otherwise, manually map the real MCP field names.
        let text = raw
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("content").and_then(|v| v.as_str()))
            .unwrap_or("");

        // Resolve `id` from the response. This is the physical primary key and
        // the authoritative source for agent attribution.
        let raw_id = raw
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(fallback_key)
            .to_string();

        // Split `id` on `#` to extract agent_id. This is the only place agent
        // attribution is derived — never from `metadata`, which real MCP servers
        // omit from search results.
        let (key, agent_id) = match raw_id.split_once('#') {
            Some((k, agent)) => (k.to_string(), Some(agent.to_string())),
            None => (raw_id.clone(), None),
        };

        // If the response also carries an explicit `key` field, prefer it as the
        // logical key (the `id`-derived key is the physical identifier).
        let logical_key = raw
            .get("key")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or(key);

        let timestamp = raw
            .get("created_at")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("timestamp").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();

        let importance = raw.get("importance").and_then(|v| v.as_f64()).or_else(|| {
            raw.get("metadata")
                .and_then(|m| m.get("importance"))
                .and_then(|v| v.as_f64())
        });

        Some(MemoryEntry {
            id: raw_id,
            key: logical_key,
            content: text.to_string(),
            category: MemoryCategory::Core,
            timestamp,
            session_id: None,
            score: None,
            namespace: self.namespace.clone(),
            importance,
            superseded_by: None,
            agent_alias: None,
            agent_id,
        })
    }
}

impl ::zeroclaw_api::attribution::Attributable for HyperMemory {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Memory(
            ::zeroclaw_api::attribution::MemoryKind::HyperMemory,
        )
    }
    fn alias(&self) -> &str {
        self.name
    }
}

#[async_trait]
impl Memory for HyperMemory {
    fn name(&self) -> &str {
        self.name
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let path = self.build_memory_path(key);
        self.call_tool(
            "save_memory",
            serde_json::json!({
                "id": key,
                "text": content,
                "path": path,
                "scope": "project",
                "project": "hyperion",
                "domain": "equity_trading",
                "category": category.to_string(),
            }),
        )
        .await?;
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let result = self
            .call_tool(
                "search_memory",
                serde_json::json!({
                    "query": query,
                    "top_k": limit,
                    "project": "hyperion",
                }),
            )
            .await?;

        let text = Self::extract_text_from_result(&result);
        if text.is_empty() {
            return Ok(Vec::new());
        }

        // Try to parse as JSON array of entries; fall back to single entry.
        let entries: Vec<MemoryEntry> =
            if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                parsed
                    .iter()
                    .filter_map(|v| self.parse_memory_entry(v, query))
                    .collect()
            } else {
                match self.parse_memory_entry(&serde_json::Value::String(text.clone()), query) {
                    Some(entry) => vec![entry],
                    None => Vec::new(),
                }
            };

        Ok(entries)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let result = self
            .call_tool(
                "get_memory",
                serde_json::json!({
                    "id": key,
                    "project": "hyperion",
                }),
            )
            .await?;

        let text = Self::extract_text_from_result(&result);
        if text.is_empty() || text == "null" {
            return Ok(None);
        }

        // Try parsing the MCP response as a structured value first.
        let raw: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text.clone()));

        Ok(self.parse_memory_entry(&raw, key))
    }

    /// Explicit agent-scoped retrieval: fetch the record by key and verify
    /// the returned `agent_id` matches the requested agent. Returns `None`
    /// if the key doesn't exist or belongs to a different agent.
    ///
    /// This overrides the trait default which naively calls `get(key)` and
    /// filters locally — that is incorrect for remote MCP backends where
    /// multiple agents may store records under the same logical key.
    async fn get_for_agent(
        &self,
        key: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        // Strong isolation: encode agent_id directly into the physical key
        // so that even MCP servers that ignore metadata filters cannot serve
        // the wrong agent's data.
        let scoped_id = format!("{key}#{agent_id}");

        let result = self
            .call_tool(
                "get_memory",
                serde_json::json!({
                    "id": scoped_id,
                    "project": "hyperion",
                }),
            )
            .await?;

        let text = Self::extract_text_from_result(&result);
        if text.is_empty() || text == "null" {
            return Ok(None);
        }

        let raw: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text.clone()));

        let entry = self.parse_memory_entry(&raw, key);

        // Defensive double-check: verify the returned entry's agent_id
        // matches the requested agent. This catches compromised or
        // misconfigured gateways that return the wrong agent's data.
        if let Some(ref e) = entry
            && e.agent_id.as_deref() != Some(agent_id)
        {
            return Ok(None);
        }

        Ok(entry)
    }

    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let result = self
            .call_tool(
                "list_memories",
                serde_json::json!({
                    "project": "hyperion",
                }),
            )
            .await?;

        let text = Self::extract_text_from_result(&result);
        if text.is_empty() {
            return Ok(Vec::new());
        }

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        let entries: Vec<MemoryEntry> = parsed
            .iter()
            .filter_map(|v| self.parse_memory_entry(v, ""))
            .collect();
        Ok(entries)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let result = self
            .call_tool(
                "delete_memory",
                serde_json::json!({
                    "id": key,
                    "project": "hyperion",
                }),
            )
            .await?;

        let text = Self::extract_text_from_result(&result);
        Ok(text.to_lowercase().contains("deleted") || text.to_lowercase().contains("true"))
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
        // Strong isolation: delete the physically unique agent-scoped key.
        let scoped_id = format!("{key}#{agent_id}");

        let result = self
            .call_tool(
                "delete_memory",
                serde_json::json!({
                    "id": scoped_id,
                    "project": "hyperion",
                }),
            )
            .await?;

        let text = Self::extract_text_from_result(&result);
        Ok(text.to_lowercase().contains("deleted") || text.to_lowercase().contains("true"))
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let entries = self.list(None, None).await?;
        Ok(entries.len())
    }

    async fn health_check(&self) -> bool {
        // Quick probe: send a lightweight list_memories call.
        self.call_tool(
            "list_memories",
            serde_json::json!({
                "project": "hyperion",
                "top_k": 1,
            }),
        )
        .await
        .is_ok()
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        _namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // Strong isolation: when an agent_id is provided, encode it into the
        // physical path and the `id` field so that each agent's record is
        // unique on the remote and retrievable by a deterministic physical key.
        let (path, record_id) = match agent_id {
            Some(aid) => (
                format!("{}#{aid}", self.build_memory_path(key)),
                format!("{key}#{aid}"),
            ),
            None => (self.build_memory_path(key), key.to_string()),
        };

        let mut args = serde_json::json!({
            "id": record_id,
            "text": content,
            "path": path,
            "scope": "project",
            "project": "hyperion",
            "domain": "equity_trading",
            "category": category.to_string(),
        });

        // Attach importance if provided.
        if let Some(imp) = importance {
            args["importance"] = serde_json::json!(imp);
        }

        // Attach agent_id as metadata for per-agent isolation at the
        // MCP gateway level.
        if let Some(aid) = agent_id {
            args["metadata"] = serde_json::json!({ "agent_id": aid });
        }

        // session_id is not part of the MCP contract but we include it
        // in metadata when present for future gateway support.
        if let Some(sid) = session_id {
            if let Some(meta) = args.get_mut("metadata") {
                meta["session_id"] = serde_json::json!(sid);
            } else {
                args["metadata"] = serde_json::json!({ "session_id": sid });
            }
        }

        self.call_tool("save_memory", args).await?;
        Ok(())
    }

    async fn recall_for_agents(
        &self,
        allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        // When an agent filter is requested, over-fetch candidates from the
        // remote MCP (5× the limit, minimum 100) to compensate for local
        // post-filtering truncation. Without this, the MCP's `top_k` limit
        // may cut off matching records that rank behind non-matching ones.
        let fetch_limit = if allowed_agent_ids.is_empty() {
            limit
        } else {
            (limit.saturating_mul(5)).max(100)
        };

        // Build the search_memory call with `allowed_agents` for server-side
        // pre-filtering. MCP servers that support this parameter can perform
        // the filter before applying `top_k`, eliminating the truncation risk
        // entirely. Servers that don't support it will simply ignore the field
        // and we fall back to local post-filtering below.
        let mut search_args = serde_json::json!({
            "query": query,
            "top_k": fetch_limit,
            "project": "hyperion",
        });

        if !allowed_agent_ids.is_empty() {
            search_args["allowed_agents"] = serde_json::json!(allowed_agent_ids);
        }

        let result = self.call_tool("search_memory", search_args).await?;

        let text = Self::extract_text_from_result(&result);
        if text.is_empty() {
            return Ok(Vec::new());
        }

        let entries: Vec<MemoryEntry> =
            if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                parsed
                    .iter()
                    .filter_map(|v| self.parse_memory_entry(v, query))
                    .collect()
            } else {
                match self.parse_memory_entry(&serde_json::Value::String(text.clone()), query) {
                    Some(entry) => vec![entry],
                    None => Vec::new(),
                }
            };

        // If no agent filter requested, return everything up to `limit`.
        if allowed_agent_ids.is_empty() {
            return Ok(entries.into_iter().take(limit).collect());
        }

        // Post-filter: only keep entries whose agent_id (extracted from `id`)
        // matches one of the allowed agents. Entries with `agent_id == None`
        // are public/unscoped memory and are always allowed through (they
        // belong to no agent, so no isolation boundary applies).
        // This is a safety net for MCP servers that ignore `allowed_agents`.
        let filtered: Vec<MemoryEntry> = entries
            .into_iter()
            .filter(|e| {
                match e.agent_id.as_deref() {
                    // Public/unscoped memory: always pass through.
                    None => true,
                    // Agent-scoped memory: only pass if the agent is in the allowlist.
                    Some(aid) => allowed_agent_ids.contains(&aid),
                }
            })
            .take(limit)
            .collect();
        Ok(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: build a JSON-RPC success response wrapping MCP content.
    fn mcp_response(body: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    { "type": "text", "text": serde_json::to_string(&body).unwrap() }
                ]
            }
        })
    }

    /// Helper: build a JSON-RPC success response with plain text.
    fn mcp_text_response(text: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    { "type": "text", "text": text }
                ]
            }
        })
    }

    // ── Test 1: store_with_agent sends correct id, path, text, metadata.agent_id ──

    #[tokio::test]
    async fn store_with_agent_sends_correct_id_path_text_and_metadata() {
        let server = MockServer::start().await;

        // The mock verifies the JSON body of the incoming request.
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_text_response("ok")))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        mem.store_with_agent(
            "market_bias",
            "AAPL bias bullish",
            MemoryCategory::Core,
            None,
            None,
            None,
            Some("agent-42"),
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);

        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let args = &body["params"]["arguments"];

        // Verify `text` equals the content.
        assert_eq!(args["text"].as_str(), Some("AAPL bias bullish"));
        // Verify `path` includes the `#agent_id` suffix for strong isolation.
        assert_eq!(
            args["path"].as_str(),
            Some("/trading/equity/market_bias#agent-42")
        );
        // Verify `id` includes the `#agent_id` suffix (CRUD closure).
        assert_eq!(args["id"].as_str(), Some("market_bias#agent-42"));
        // Verify `metadata.agent_id` is set.
        assert_eq!(args["metadata"]["agent_id"].as_str(), Some("agent-42"));
    }

    // ── Test 2: recall sends top_k and maps entries correctly ──

    #[tokio::test]
    async fn recall_sends_top_k_and_maps_entries() {
        let server = MockServer::start().await;

        // Simulate real MCP slim responses: no metadata.agent_id, agent
        // attribution encoded in the `id` field via `#` separator.
        let entries = serde_json::json!([
            {
                "id": "bias#agent-a",
                "text": "AAPL bullish",
                "created_at": "2026-01-01T00:00:00Z"
            },
            {
                "id": "bias2#agent-b",
                "text": "GOOG bearish",
                "created_at": "2026-01-02T00:00:00Z"
            }
        ]);

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entries)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let results = mem.recall("bias", 5, None, None, None).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "AAPL bullish");
        assert_eq!(results[0].key, "bias");
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-a"));
        assert_eq!(results[1].content, "GOOG bearish");
        assert_eq!(results[1].key, "bias2");
        assert_eq!(results[1].agent_id.as_deref(), Some("agent-b"));

        // Verify top_k was sent correctly.
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["params"]["arguments"]["top_k"].as_u64(), Some(5));
    }

    // ── Test 3: get_for_agent isolation ──

    #[tokio::test]
    async fn get_for_agent_returns_matching_entry() {
        let server = MockServer::start().await;

        // Simulate real MCP slim response: no metadata.agent_id.
        let entry = serde_json::json!({
            "id": "bias#agent-42",
            "text": "AAPL data",
            "created_at": "2026-01-01T00:00:00Z"
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entry)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get_for_agent("bias", "agent-42").await.unwrap();
        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.content, "AAPL data");
        assert_eq!(entry.key, "bias");
        assert_eq!(entry.agent_id.as_deref(), Some("agent-42"));

        // Verify the `id` sent to MCP includes the `#agent_id` suffix.
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["params"]["arguments"]["id"].as_str(),
            Some("bias#agent-42")
        );
    }

    #[tokio::test]
    async fn get_for_agent_returns_none_for_mismatched_agent() {
        let server = MockServer::start().await;

        // Server returns empty/null because the scoped key doesn't exist.
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_text_response("null")))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get_for_agent("bias", "agent-42").await.unwrap();
        assert!(
            result.is_none(),
            "should return None when remote has no record for this scoped key"
        );

        // Verify the `id` sent was the scoped key.
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["params"]["arguments"]["id"].as_str(),
            Some("bias#agent-42")
        );
    }

    // ── Test 4: forget_for_agent passes agent_id in metadata ──

    #[tokio::test]
    async fn forget_for_agent_uses_scoped_key() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_text_response("deleted")))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let deleted = mem.forget_for_agent("bias", "agent-42").await.unwrap();
        assert!(deleted);

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let args = &body["params"]["arguments"];
        // Strong isolation: `id` includes `#agent_id` suffix.
        assert_eq!(args["id"].as_str(), Some("bias#agent-42"));
        // No metadata passthrough — isolation is physical, not metadata-based.
        assert!(args.get("metadata").is_none());
    }

    // ── Test 5: recall_for_agents filters by allowed_agent_ids ──

    #[tokio::test]
    async fn recall_for_agents_filters_by_allowed_ids() {
        let server = MockServer::start().await;

        // Simulate real MCP slim responses: agent attribution in `id` only.
        let entries = serde_json::json!([
            {
                "id": "bias#agent-a",
                "text": "AAPL bullish",
                "created_at": "2026-01-01T00:00:00Z"
            },
            {
                "id": "bias2#agent-b",
                "text": "GOOG bearish",
                "created_at": "2026-01-02T00:00:00Z"
            },
            {
                "id": "bias3#agent-c",
                "text": "TSLA neutral",
                "created_at": "2026-01-03T00:00:00Z"
            },
            {
                "id": "bias4",
                "text": "public entry",
                "created_at": "2026-01-04T00:00:00Z"
            }
        ]);

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entries)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let results = mem
            .recall_for_agents(&["agent-a", "agent-c"], "bias", 5, None, None, None)
            .await
            .unwrap();

        // Should include agent-a, agent-c, and the public entry (no agent_id).
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].content, "AAPL bullish");
        assert_eq!(results[0].key, "bias");
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-a"));
        assert_eq!(results[1].content, "TSLA neutral");
        assert_eq!(results[1].key, "bias3");
        assert_eq!(results[1].agent_id.as_deref(), Some("agent-c"));
        assert_eq!(results[2].content, "public entry");
        assert_eq!(results[2].key, "bias4");
        assert_eq!(results[2].agent_id, None);

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let args = &body["params"]["arguments"];
        // Verify over-fetch: top_k should be (5*5).max(100) = 100.
        assert_eq!(args["top_k"].as_u64(), Some(100));
        // Verify allowed_agents is passed for server-side filtering.
        let allowed: Vec<&str> = args["allowed_agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(allowed, vec!["agent-a", "agent-c"]);
    }

    #[tokio::test]
    async fn recall_for_agents_respects_limit_after_filter() {
        let server = MockServer::start().await;

        // Simulate real MCP slim responses: agent attribution in `id` only.
        let entries = serde_json::json!([
            {
                "id": "bias#agent-a",
                "text": "AAPL 1",
                "created_at": "2026-01-01T00:00:00Z"
            },
            {
                "id": "bias2#agent-a",
                "text": "AAPL 2",
                "created_at": "2026-01-02T00:00:00Z"
            },
            {
                "id": "bias3#agent-a",
                "text": "AAPL 3",
                "created_at": "2026-01-03T00:00:00Z"
            }
        ]);

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entries)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let results = mem
            .recall_for_agents(&["agent-a"], "AAPL", 2, None, None, None)
            .await
            .unwrap();

        // Even though 3 entries match, limit=2 should be respected.
        assert_eq!(results.len(), 2);
    }

    // ── Test 6: store (without agent) sends correct path ──

    #[tokio::test]
    async fn store_sends_correct_trading_equity_path() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_text_response("ok")))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        mem.store(
            "portfolio_state",
            "AAPL: 100 shares",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let args = &body["params"]["arguments"];
        assert_eq!(args["text"].as_str(), Some("AAPL: 100 shares"));
        assert_eq!(
            args["path"].as_str(),
            Some("/trading/equity/portfolio_state")
        );
        // Verify `id` equals the key (CRUD closure for unscoped store).
        assert_eq!(args["id"].as_str(), Some("portfolio_state"));
    }

    // ── Test 7: get returns entry when found ──

    #[tokio::test]
    async fn get_returns_entry_when_found() {
        let server = MockServer::start().await;

        let entry = serde_json::json!({
            "id": "e1",
            "key": "portfolio_state",
            "text": "AAPL: 100 shares",
            "created_at": "2026-01-01T00:00:00Z"
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entry)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get("portfolio_state").await.unwrap();
        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.content, "AAPL: 100 shares");
    }

    // ── Test 8: get returns none when not found ──

    #[tokio::test]
    async fn get_returns_none_when_empty() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_text_response("null")))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    // ── Test 9: health_check returns true on success ──

    #[tokio::test]
    async fn health_check_returns_true_when_server_responds() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_text_response("[]")))
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        assert!(mem.health_check().await);
    }

    // ── Test 10: recall_for_agents with empty allowed list returns all ──

    #[tokio::test]
    async fn recall_for_agents_empty_allowed_returns_all_up_to_limit() {
        let server = MockServer::start().await;

        let entries = serde_json::json!([
            { "id": "e1", "key": "a", "text": "A", "created_at": "" },
            { "id": "e2", "key": "b", "text": "B", "created_at": "" },
            { "id": "e3", "key": "c", "text": "C", "created_at": "" }
        ]);

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entries)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let results = mem
            .recall_for_agents(&[], "test", 2, None, None, None)
            .await
            .unwrap();

        // Should return up to limit=2 entries.
        assert_eq!(results.len(), 2);

        // Verify top_k == limit (no over-fetch when allowed is empty) and
        // no `allowed_agents` field is sent.
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["params"]["arguments"]["top_k"].as_u64(), Some(2));
        assert!(
            body["params"]["arguments"].get("allowed_agents").is_none(),
            "allowed_agents should not be sent when the list is empty"
        );
    }

    // ── Test 11: parse_memory_entry enforces `#` splitting on fast-path ──

    #[tokio::test]
    async fn parse_memory_entry_enforces_hash_splitting_on_deserialization_fast_path() {
        let mem = HyperMemory::new("test");

        // Simulate a fully-shaped MemoryEntry JSON where the `id` contains `#`
        // but the top-level `key` and `agent_id` fields are wrong/misleading.
        // The fast-path deserialization would previously return this as-is,
        // bypassing `#` splitting. After the fix, `#` splitting must override.
        let raw = serde_json::json!({
            "id": "bias#agent-42",
            "key": "wrong_key",
            "content": "AAPL bullish",
            "category": "core",
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": null,
            "score": null,
            "namespace": "default",
            "importance": null,
            "superseded_by": null,
            "agent_alias": null,
            "agent_id": null   // malicious/incorrect — should be overridden by `#` split
        });

        let entry = mem.parse_memory_entry(&raw, "fallback").unwrap();

        // The `#` splitting must override the deserialized fields.
        assert_eq!(
            entry.key, "bias",
            "`#` splitting must override the `key` field"
        );
        assert_eq!(
            entry.agent_id.as_deref(),
            Some("agent-42"),
            "`#` splitting must set `agent_id` from the `id` field"
        );
        // The `id` should remain the original physical key.
        assert_eq!(entry.id, "bias#agent-42");
    }

    #[tokio::test]
    async fn parse_memory_entry_preserves_unscoped_entry_without_hash() {
        let mem = HyperMemory::new("test");

        // A fully-shaped MemoryEntry without `#` in the id — should pass
        // through the fast-path as unscoped even if the payload claims an
        // agent_id. The physical id is the source of truth.
        let raw = serde_json::json!({
            "id": "public_fact",
            "key": "public_fact",
            "content": "sky is blue",
            "category": "core",
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": null,
            "score": null,
            "namespace": "default",
            "importance": null,
            "superseded_by": null,
            "agent_alias": null,
            "agent_id": "agent-42"
        });

        let entry = mem.parse_memory_entry(&raw, "fallback").unwrap();
        assert_eq!(entry.key, "public_fact");
        assert_eq!(entry.agent_id, None);
    }

    // ── Test 12: get_for_agent defensive double-check ──

    #[tokio::test]
    async fn get_for_agent_returns_none_when_remote_returns_wrong_agent() {
        let server = MockServer::start().await;

        // Simulate a compromised gateway returning data for agent-999
        // when we asked for agent-42's scoped key.
        let entry = serde_json::json!({
            "id": "bias#agent-999",
            "text": "secret data from agent-999",
            "created_at": "2026-01-01T00:00:00Z"
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entry)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get_for_agent("bias", "agent-42").await.unwrap();

        assert!(
            result.is_none(),
            "get_for_agent must return None when the remote returns a mismatched agent's data"
        );
    }

    #[tokio::test]
    async fn get_for_agent_returns_none_when_remote_returns_unscoped_entry() {
        let server = MockServer::start().await;

        // Simulate a gateway returning an unscoped entry (no `#` in id,
        // agent_id = None) when we asked for agent-42's scoped key.
        let entry = serde_json::json!({
            "id": "bias",
            "text": "unscoped public data",
            "created_at": "2026-01-01T00:00:00Z"
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entry)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get_for_agent("bias", "agent-42").await.unwrap();

        assert!(
            result.is_none(),
            "get_for_agent must return None when the remote returns an unscoped entry \
             (agent_id != requested agent)"
        );
    }

    #[tokio::test]
    async fn get_for_agent_returns_none_when_memoryentry_payload_claims_agent_without_hash() {
        let server = MockServer::start().await;

        // Simulate a MemoryEntry-shaped payload that claims the requested
        // agent_id even though the physical id is unscoped. The parser must
        // ignore that claimed agent_id, causing the defensive check to reject.
        let entry = serde_json::json!({
            "id": "bias",
            "key": "bias",
            "content": "unscoped data with forged agent_id",
            "category": "core",
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": null,
            "score": null,
            "namespace": "default",
            "importance": null,
            "superseded_by": null,
            "agent_alias": null,
            "agent_id": "agent-42"
        });

        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(mcp_response(entry)))
            .expect(1)
            .mount(&server)
            .await;

        let mem = HyperMemory::with_url("test", &server.uri());
        let result = mem.get_for_agent("bias", "agent-42").await.unwrap();

        assert!(
            result.is_none(),
            "get_for_agent must reject MemoryEntry-shaped responses whose \
             physical id is unscoped even if agent_id claims a match"
        );
    }
}
