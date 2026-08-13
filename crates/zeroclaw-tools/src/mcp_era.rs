//! Dual-era MCP peer classification and modern-wire helpers.
//!
//! Protocol version `2026-07-28` removed the `initialize` handshake. This
//! module is the seam that must exist *before* [`MCP_PROTOCOL_VERSION`] can
//! move: probe once per server, resolve a [`PeerEra`], and branch handshake /
//! session / transport on that one value. Stage 2 speaks the modern POST
//! headers and per-request `_meta` only on [`PeerEra::Modern`]; the Legacy
//! arm is unchanged. Do not bump [`MCP_PROTOCOL_VERSION`] here.
//!
//! [`MCP_PROTOCOL_VERSION`]: crate::mcp_protocol::MCP_PROTOCOL_VERSION

use base64::Engine;
use serde_json::json;

use crate::mcp_protocol::{JsonRpcError, JsonRpcResponse, MCP_PROTOCOL_VERSION};

/// First modern (per-request `_meta`) revision. Used only in the
/// `server/discover` probe — not a bump of [`MCP_PROTOCOL_VERSION`].
///
/// [`MCP_PROTOCOL_VERSION`]: crate::mcp_protocol::MCP_PROTOCOL_VERSION
pub const MCP_MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// Latest handshake-era revision this client classifies as [`PeerEra::Legacy`].
pub const MCP_LEGACY_LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Streamable HTTP introduction; still handshake-era.
pub const MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION: &str = "2025-03-26";

/// Revisions this client can name. Spoken wire still follows [`PeerEra`]:
/// handshake-era dates use initialize; [`MCP_MODERN_PROTOCOL_VERSION`] uses
/// per-request `_meta`. [`MCP_PROTOCOL_VERSION`] itself is not bumped here.
pub const KNOWN_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_PROTOCOL_VERSION,
    MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION,
    MCP_LEGACY_LATEST_PROTOCOL_VERSION,
    MCP_MODERN_PROTOCOL_VERSION,
];

/// `_meta` key for the per-request protocol version (`2026-07-28`).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key for client identity on each modern request.
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
/// `_meta` key for client capabilities on each modern request.
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// Streamable HTTP header that must match [`META_PROTOCOL_VERSION`].
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
/// Streamable HTTP header mirroring JSON-RPC `method`.
pub const MCP_METHOD_HEADER: &str = "Mcp-Method";
/// Streamable HTTP header mirroring `params.name` or `params.uri`.
pub const MCP_NAME_HEADER: &str = "Mcp-Name";

/// JSON-RPC server-error codes introduced in `2026-07-28` (spec range
/// `-32020`..`-32099`). A response carrying one of these identifies a
/// modern server even when the HTTP status is `400`.
pub const HEADER_MISMATCH: i32 = -32020;
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i32 = -32021;
pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// Handshake-based (`2025-11-25` and earlier) vs per-request-metadata
/// (`2026-07-28` and later) MCP revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerEra {
    /// `initialize` + `notifications/initialized` session.
    Legacy,
    /// Stateless; version and capabilities ride in `_meta`.
    Modern,
}

/// How a recorded version relates to the revisions this client knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionQuality {
    /// Peer named a revision in [`KNOWN_PROTOCOL_VERSIONS`].
    Known,
    /// Peer named a well-formed date this client has no arm for.
    UnknownRevision,
    /// `protocolVersion` was missing or not a string.
    Malformed,
}

/// `server/discover` listed versions, but none match a revision we know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverNegotiateError {
    Empty,
    NoOverlap { advertised: Vec<String> },
}

impl std::fmt::Display for DiscoverNegotiateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => {
                write!(
                    f,
                    "server/discover returned an empty supportedVersions list"
                )
            }
            Self::NoOverlap { advertised } => write!(
                f,
                "no mutually supported MCP protocol version (peer advertised {advertised:?}; \
                 known {KNOWN_PROTOCOL_VERSIONS:?})"
            ),
        }
    }
}

/// Resolved protocol this client recorded for one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerProtocol {
    pub era: PeerEra,
    /// Version selected for subsequent requests (snapped to a known
    /// revision when the peer advertised something we do not have an arm
    /// for).
    pub version: String,
    /// Version the peer actually advertised, before snapping. For a
    /// malformed field this is the raw JSON (or `"<missing>"`).
    pub advertised: String,
    pub quality: VersionQuality,
}

impl PeerProtocol {
    /// Default for test fixtures and a well-formed pin of our client version.
    pub fn legacy_default() -> Self {
        Self {
            era: PeerEra::Legacy,
            version: MCP_PROTOCOL_VERSION.to_string(),
            advertised: MCP_PROTOCOL_VERSION.to_string(),
            quality: VersionQuality::Known,
        }
    }

    /// Well-formed pin of the first modern revision. Used by the
    /// `server/discover` probe and by tests of the modern transport arm.
    pub fn modern_default() -> Self {
        Self {
            era: PeerEra::Modern,
            version: MCP_MODERN_PROTOCOL_VERSION.to_string(),
            advertised: MCP_MODERN_PROTOCOL_VERSION.to_string(),
            quality: VersionQuality::Known,
        }
    }

    /// Classify a version string the peer named (initialize or discover).
    pub fn classify(advertised: &str) -> Self {
        match advertised {
            MCP_PROTOCOL_VERSION
            | MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION
            | MCP_LEGACY_LATEST_PROTOCOL_VERSION => Self {
                era: PeerEra::Legacy,
                version: advertised.to_string(),
                advertised: advertised.to_string(),
                quality: VersionQuality::Known,
            },
            MCP_MODERN_PROTOCOL_VERSION => Self {
                era: PeerEra::Modern,
                version: advertised.to_string(),
                advertised: advertised.to_string(),
                quality: VersionQuality::Known,
            },
            other => {
                let (era, snapped) = nearest_known(other);
                Self {
                    era,
                    version: snapped.to_string(),
                    advertised: other.to_string(),
                    quality: VersionQuality::UnknownRevision,
                }
            }
        }
    }

    /// Negotiate from `supportedVersions`. Succeeds only when the peer
    /// listed at least one revision in [`KNOWN_PROTOCOL_VERSIONS`].
    pub fn from_discover_supported(supported: &[String]) -> Result<Self, DiscoverNegotiateError> {
        if supported.is_empty() {
            return Err(DiscoverNegotiateError::Empty);
        }
        let overlap: Vec<&str> = KNOWN_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .filter(|known| supported.iter().any(|advertised| advertised == known))
            .collect();
        if overlap.is_empty() {
            return Err(DiscoverNegotiateError::NoOverlap {
                advertised: supported.to_vec(),
            });
        }
        let selected = if overlap.contains(&MCP_MODERN_PROTOCOL_VERSION) {
            MCP_MODERN_PROTOCOL_VERSION
        } else {
            overlap.iter().max().copied().expect("overlap is non-empty")
        };
        // Spoken era follows the selected revision. Handshake-era overlap is
        // Legacy (initialize is legal); only a modern date selects Modern.
        Ok(Self::classify(selected))
    }

    /// Parse `initialize.result.protocolVersion`. Missing or non-string
    /// values are malformed (conservative fallback); a well-formed unknown
    /// date is an unknown revision (snap to nearest known).
    pub fn from_initialize_field(value: Option<&serde_json::Value>) -> Self {
        match value {
            None => Self {
                era: PeerEra::Legacy,
                version: MCP_PROTOCOL_VERSION.to_string(),
                advertised: "<missing>".to_string(),
                quality: VersionQuality::Malformed,
            },
            Some(serde_json::Value::String(advertised)) => Self::classify(advertised),
            Some(other) => Self {
                era: PeerEra::Legacy,
                version: MCP_PROTOCOL_VERSION.to_string(),
                advertised: other.to_string(),
                quality: VersionQuality::Malformed,
            },
        }
    }
}

/// ISO-8601 protocol dates sort lexicographically. Unknown dates snap to
/// the nearest classified revision.
fn nearest_known(advertised: &str) -> (PeerEra, &'static str) {
    if advertised >= MCP_MODERN_PROTOCOL_VERSION {
        (PeerEra::Modern, MCP_MODERN_PROTOCOL_VERSION)
    } else if advertised >= MCP_LEGACY_LATEST_PROTOCOL_VERSION {
        (PeerEra::Legacy, MCP_LEGACY_LATEST_PROTOCOL_VERSION)
    } else if advertised >= MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION {
        (PeerEra::Legacy, MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION)
    } else {
        (PeerEra::Legacy, MCP_PROTOCOL_VERSION)
    }
}

/// `true` for the JSON-RPC codes that identify a modern MCP server.
pub fn is_recognized_modern_error(code: i32) -> bool {
    matches!(
        code,
        HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY | UNSUPPORTED_PROTOCOL_VERSION
    )
}

/// Pull `data.supported` out of an `UnsupportedProtocolVersionError`.
pub fn versions_from_unsupported_error(error: &JsonRpcError) -> Vec<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("supported"))
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// If `resp` is a recognized modern JSON-RPC error, return it.
pub fn take_recognized_modern_error(resp: JsonRpcResponse) -> Option<JsonRpcResponse> {
    let code = resp.error.as_ref()?.code;
    is_recognized_modern_error(code).then_some(resp)
}

/// Per-request `_meta` object for a modern-era JSON-RPC call.
pub fn client_request_meta(version: &str) -> serde_json::Value {
    json!({
        META_PROTOCOL_VERSION: version,
        META_CLIENT_INFO: {
            "name": "zeroclaw",
            "version": env!("CARGO_PKG_VERSION")
        },
        META_CLIENT_CAPABILITIES: {
            "resources": {},
            "prompts": {}
        }
    })
}

/// Merge caller `_meta` extensions with the negotiated protocol keys.
/// `protocolVersion` and `clientCapabilities` always come from `version`;
/// caller keys that are not those required fields are preserved.
pub fn attach_request_meta(params: serde_json::Value, version: &str) -> serde_json::Value {
    let required = client_request_meta(version);
    let merged_meta = match params.get("_meta") {
        Some(serde_json::Value::Object(existing)) => {
            let mut merged = existing.clone();
            if let serde_json::Value::Object(required) = required {
                merged.insert(
                    META_PROTOCOL_VERSION.to_string(),
                    required
                        .get(META_PROTOCOL_VERSION)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                );
                merged.insert(
                    META_CLIENT_CAPABILITIES.to_string(),
                    required
                        .get(META_CLIENT_CAPABILITIES)
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                );
                merged
                    .entry(META_CLIENT_INFO.to_string())
                    .or_insert_with(|| {
                        required
                            .get(META_CLIENT_INFO)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null)
                    });
            }
            serde_json::Value::Object(merged)
        }
        _ => required,
    };
    match params {
        serde_json::Value::Object(mut map) => {
            map.insert("_meta".to_string(), merged_meta);
            serde_json::Value::Object(map)
        }
        serde_json::Value::Null => json!({ "_meta": merged_meta }),
        other => other,
    }
}

/// Body field mirrored into `Mcp-Name` for the methods that require it.
pub fn mcp_name_header_source<'a>(
    method: &str,
    params: Option<&'a serde_json::Value>,
) -> Option<&'a str> {
    let params = params?.as_object()?;
    match method {
        "tools/call" | "prompts/get" => params.get("name")?.as_str(),
        "resources/read" => params.get("uri")?.as_str(),
        _ => None,
    }
}

/// Encode a value for an MCP mirrored header (`Mcp-Name` / `Mcp-Param-*`).
pub fn encode_mcp_header_value(value: &str) -> String {
    if mcp_header_needs_encoding(value) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
        format!("=?base64?{encoded}?=")
    } else {
        value.to_string()
    }
}

fn mcp_header_needs_encoding(value: &str) -> bool {
    if value.starts_with("=?base64?") && value.ends_with("?=") {
        return true;
    }
    if value.starts_with(char::is_whitespace) || value.ends_with(char::is_whitespace) {
        return true;
    }
    !value
        .chars()
        .all(|c| c == '\t' || ('\u{0020}'..='\u{007E}').contains(&c))
}

/// `cacheScope` on a CacheableResult (`tools/list` and the other list/read RPCs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    Public,
    Private,
}

/// Freshness hint parsed from a modern list/read result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheHints {
    pub ttl_ms: u64,
    pub cache_scope: CacheScope,
}

/// Parse `ttlMs` + `cacheScope`. Unknown or partial values yield `None`
/// (do not cache) rather than guessing.
pub fn cache_hints_from_result(result: &serde_json::Value) -> Option<CacheHints> {
    let ttl_ms = result.get("ttlMs")?.as_u64()?;
    let cache_scope = match result.get("cacheScope")?.as_str()? {
        "public" => CacheScope::Public,
        "private" => CacheScope::Private,
        _ => return None,
    };
    Some(CacheHints {
        ttl_ms,
        cache_scope,
    })
}

/// Local cache TTL for a modern list result. `ttlMs == 0` means do not cache.
/// Both `public` and `private` may be stored on this connection: the
/// `McpServer` handle is already per-peer, so `private` is not shared.
pub fn local_cache_ttl(hints: CacheHints) -> Option<std::time::Duration> {
    if hints.ttl_ms == 0 {
        return None;
    }
    match hints.cache_scope {
        CacheScope::Public | CacheScope::Private => {
            Some(std::time::Duration::from_millis(hints.ttl_ms))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_protocol::JSONRPC_VERSION;
    use serde_json::json;

    #[test]
    fn known_legacy_dates_stay_legacy() {
        for version in [
            MCP_PROTOCOL_VERSION,
            MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION,
            MCP_LEGACY_LATEST_PROTOCOL_VERSION,
        ] {
            let peer = PeerProtocol::classify(version);
            assert_eq!(peer.era, PeerEra::Legacy);
            assert_eq!(peer.version, version);
            assert_eq!(peer.quality, VersionQuality::Known);
        }
    }

    #[test]
    fn known_modern_date_is_modern() {
        let peer = PeerProtocol::classify(MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.quality, VersionQuality::Known);
    }

    #[test]
    fn unknown_future_date_snaps_to_modern() {
        let peer = PeerProtocol::classify("2027-01-01");
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.advertised, "2027-01-01");
        assert_eq!(peer.quality, VersionQuality::UnknownRevision);
    }

    #[test]
    fn unknown_mid_legacy_date_snaps_to_nearest_legacy() {
        let peer = PeerProtocol::classify("2025-06-18");
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION);
        assert_eq!(peer.quality, VersionQuality::UnknownRevision);
    }

    #[test]
    fn unknown_ancient_date_snaps_to_original_legacy() {
        let peer = PeerProtocol::classify("2023-01-01");
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_PROTOCOL_VERSION);
        assert_eq!(peer.quality, VersionQuality::UnknownRevision);
    }

    #[test]
    fn discover_prefers_modern_when_listed() {
        let peer = PeerProtocol::from_discover_supported(&[
            MCP_LEGACY_LATEST_PROTOCOL_VERSION.to_string(),
            MCP_MODERN_PROTOCOL_VERSION.to_string(),
        ])
        .expect("overlap");
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.quality, VersionQuality::Known);
    }

    #[test]
    fn discover_empty_supported_is_error() {
        assert_eq!(
            PeerProtocol::from_discover_supported(&[]).unwrap_err(),
            DiscoverNegotiateError::Empty
        );
    }

    #[test]
    fn discover_unknown_only_list_is_incompatible() {
        let err = PeerProtocol::from_discover_supported(&["2027-06-01".into()]).unwrap_err();
        assert_eq!(
            err,
            DiscoverNegotiateError::NoOverlap {
                advertised: vec!["2027-06-01".into()]
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("no mutually supported"), "got: {msg}");
        assert!(msg.contains("2027-06-01"), "got: {msg}");
    }

    #[test]
    fn initialize_missing_version_is_malformed() {
        let peer = PeerProtocol::from_initialize_field(None);
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_PROTOCOL_VERSION);
        assert_eq!(peer.advertised, "<missing>");
        assert_eq!(peer.quality, VersionQuality::Malformed);
    }

    #[test]
    fn initialize_non_string_version_is_malformed() {
        let peer = PeerProtocol::from_initialize_field(Some(&json!(42)));
        assert_eq!(peer.quality, VersionQuality::Malformed);
        assert_eq!(peer.version, MCP_PROTOCOL_VERSION);
        assert_eq!(peer.advertised, "42");
    }

    #[test]
    fn initialize_unknown_revision_snaps_and_is_not_malformed() {
        let peer = PeerProtocol::from_initialize_field(Some(&json!("2027-01-01")));
        assert_eq!(peer.quality, VersionQuality::UnknownRevision);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.advertised, "2027-01-01");
    }

    #[test]
    fn initialize_known_modern_date_classifies_modern() {
        let peer = PeerProtocol::from_initialize_field(Some(&json!(MCP_MODERN_PROTOCOL_VERSION)));
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.quality, VersionQuality::Known);
    }

    #[test]
    fn recognized_modern_error_codes() {
        assert!(is_recognized_modern_error(UNSUPPORTED_PROTOCOL_VERSION));
        assert!(is_recognized_modern_error(HEADER_MISMATCH));
        assert!(is_recognized_modern_error(
            MISSING_REQUIRED_CLIENT_CAPABILITY
        ));
        assert!(!is_recognized_modern_error(-32601));
        assert!(!is_recognized_modern_error(-32602));
    }

    #[test]
    fn unsupported_error_reads_supported_list() {
        let error = JsonRpcError {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".into(),
            data: Some(json!({
                "supported": [MCP_MODERN_PROTOCOL_VERSION, MCP_LEGACY_LATEST_PROTOCOL_VERSION],
                "requested": "1900-01-01"
            })),
        };
        let versions = versions_from_unsupported_error(&error);
        assert_eq!(
            versions,
            vec![
                MCP_MODERN_PROTOCOL_VERSION.to_string(),
                MCP_LEGACY_LATEST_PROTOCOL_VERSION.to_string()
            ]
        );
    }

    #[test]
    fn take_recognized_modern_error_ignores_method_not_found() {
        let resp = JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(json!(0)),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "Method not found".into(),
                data: None,
            }),
        };
        assert!(take_recognized_modern_error(resp).is_none());
    }

    #[test]
    fn take_recognized_modern_error_keeps_unsupported_version() {
        let resp = JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(json!(0)),
            result: None,
            error: Some(JsonRpcError {
                code: UNSUPPORTED_PROTOCOL_VERSION,
                message: "Unsupported protocol version".into(),
                data: Some(json!({"supported": [MCP_MODERN_PROTOCOL_VERSION]})),
            }),
        };
        assert!(take_recognized_modern_error(resp).is_some());
    }

    #[test]
    fn discover_legacy_only_overlap_is_legacy() {
        let peer =
            PeerProtocol::from_discover_supported(
                &[MCP_LEGACY_LATEST_PROTOCOL_VERSION.to_string()],
            )
            .expect("overlap");
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_LEGACY_LATEST_PROTOCOL_VERSION);
    }

    #[test]
    fn discover_modern_only_overlap_is_modern() {
        let peer =
            PeerProtocol::from_discover_supported(&[MCP_MODERN_PROTOCOL_VERSION.to_string()])
                .expect("overlap");
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
    }

    #[test]
    fn attach_request_meta_inserts_required_keys() {
        let params = attach_request_meta(json!({"name": "echo"}), MCP_MODERN_PROTOCOL_VERSION);
        let meta = params.get("_meta").expect("meta");
        assert_eq!(
            meta.get(META_PROTOCOL_VERSION).and_then(|v| v.as_str()),
            Some(MCP_MODERN_PROTOCOL_VERSION)
        );
        assert!(meta.get(META_CLIENT_INFO).is_some());
        assert!(meta.get(META_CLIENT_CAPABILITIES).is_some());
        assert_eq!(params.get("name").and_then(|v| v.as_str()), Some("echo"));
    }

    #[test]
    fn attach_request_meta_overlays_required_keys_and_keeps_extensions() {
        let params = attach_request_meta(
            json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "keep-me",
                    "io.modelcontextprotocol/clientCapabilities": {"stale": true},
                    "traceparent": "00-trace"
                }
            }),
            MCP_MODERN_PROTOCOL_VERSION,
        );
        let meta = params.get("_meta").expect("meta");
        assert_eq!(
            meta.get(META_PROTOCOL_VERSION).and_then(|v| v.as_str()),
            Some(MCP_MODERN_PROTOCOL_VERSION)
        );
        assert_eq!(
            meta.get(META_CLIENT_CAPABILITIES),
            Some(&json!({"resources": {}, "prompts": {}}))
        );
        assert_eq!(
            meta.get("traceparent").and_then(|v| v.as_str()),
            Some("00-trace")
        );
    }

    #[test]
    fn mcp_name_header_source_reads_call_and_uri() {
        assert_eq!(
            mcp_name_header_source("tools/call", Some(&json!({"name": "echo"}))),
            Some("echo")
        );
        assert_eq!(
            mcp_name_header_source("resources/read", Some(&json!({"uri": "file:///a"}))),
            Some("file:///a")
        );
        assert_eq!(mcp_name_header_source("tools/list", Some(&json!({}))), None);
    }

    #[test]
    fn encode_mcp_header_value_plain_ascii_passthrough() {
        assert_eq!(encode_mcp_header_value("get_weather"), "get_weather");
    }

    #[test]
    fn encode_mcp_header_value_non_ascii_uses_base64_sentinel() {
        assert_eq!(
            encode_mcp_header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
    }

    #[test]
    fn encode_mcp_header_value_encodes_sentinel_literal() {
        assert_eq!(
            encode_mcp_header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn cache_hints_require_both_fields() {
        assert!(cache_hints_from_result(&json!({"ttlMs": 1000})).is_none());
        assert!(cache_hints_from_result(&json!({"cacheScope": "public"})).is_none());
        let hints = cache_hints_from_result(&json!({"ttlMs": 1500, "cacheScope": "private"}))
            .expect("hints");
        assert_eq!(hints.ttl_ms, 1500);
        assert_eq!(hints.cache_scope, CacheScope::Private);
        assert_eq!(
            local_cache_ttl(hints),
            Some(std::time::Duration::from_millis(1500))
        );
        assert!(
            local_cache_ttl(CacheHints {
                ttl_ms: 0,
                cache_scope: CacheScope::Public
            })
            .is_none()
        );
    }
}
