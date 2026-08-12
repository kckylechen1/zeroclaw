//! Dual-era MCP peer classification.
//!
//! Protocol version `2026-07-28` removed the `initialize` handshake. This
//! module is the seam that must exist *before* [`MCP_PROTOCOL_VERSION`] can
//! move: probe once per server, resolve a [`PeerEra`], and keep today's
//! handshake as the [`PeerEra::Legacy`] arm.
//!
//! [`MCP_PROTOCOL_VERSION`]: crate::mcp_protocol::MCP_PROTOCOL_VERSION

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

/// Resolved protocol this client will speak to one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerProtocol {
    pub era: PeerEra,
    /// Version selected for subsequent requests (snapped to a known
    /// revision when the peer advertised something we do not have an arm
    /// for).
    pub version: String,
    /// Version the peer actually advertised, before snapping.
    pub advertised: String,
    /// `true` when [`advertised`](Self::advertised) was not a revision this
    /// client has a dedicated arm for.
    pub unknown: bool,
}

impl PeerProtocol {
    /// Default for test fixtures and omitted `initialize.protocolVersion`.
    pub fn legacy_default() -> Self {
        Self {
            era: PeerEra::Legacy,
            version: MCP_PROTOCOL_VERSION.to_string(),
            advertised: MCP_PROTOCOL_VERSION.to_string(),
            unknown: false,
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
                unknown: false,
            },
            MCP_MODERN_PROTOCOL_VERSION => Self {
                era: PeerEra::Modern,
                version: advertised.to_string(),
                advertised: advertised.to_string(),
                unknown: false,
            },
            other => {
                let (era, snapped) = nearest_known(other);
                Self {
                    era,
                    version: snapped.to_string(),
                    advertised: other.to_string(),
                    unknown: true,
                }
            }
        }
    }

    /// A successful `server/discover` (or a recognized modern error) means
    /// the peer is modern regardless of which dates it listed.
    pub fn from_discover_supported(supported: &[String]) -> Option<Self> {
        if supported.is_empty() {
            return None;
        }
        let selected = if supported.iter().any(|v| v == MCP_MODERN_PROTOCOL_VERSION) {
            MCP_MODERN_PROTOCOL_VERSION.to_string()
        } else {
            supported.iter().max().cloned().unwrap_or_default()
        };
        let mut peer = Self::classify(&selected);
        peer.era = PeerEra::Modern;
        Some(peer)
    }

    /// `initialize` result: era is already known to be legacy (the probe
    /// fell back). Record and snap the advertised `protocolVersion`.
    pub fn from_initialize_version(advertised: Option<&str>) -> Self {
        let Some(advertised) = advertised else {
            return Self::legacy_default();
        };
        let mut peer = Self::classify(advertised);
        peer.era = PeerEra::Legacy;
        if peer.version == MCP_MODERN_PROTOCOL_VERSION {
            // Handshake path cannot speak the modern wire yet.
            peer.version = MCP_LEGACY_LATEST_PROTOCOL_VERSION.to_string();
            peer.unknown = true;
        }
        peer
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
            assert!(!peer.unknown);
        }
    }

    #[test]
    fn known_modern_date_is_modern() {
        let peer = PeerProtocol::classify(MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert!(!peer.unknown);
    }

    #[test]
    fn unknown_future_date_snaps_to_modern() {
        let peer = PeerProtocol::classify("2027-01-01");
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert_eq!(peer.advertised, "2027-01-01");
        assert!(peer.unknown);
    }

    #[test]
    fn unknown_mid_legacy_date_snaps_to_nearest_legacy() {
        let peer = PeerProtocol::classify("2025-06-18");
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION);
        assert!(peer.unknown);
    }

    #[test]
    fn unknown_ancient_date_snaps_to_original_legacy() {
        let peer = PeerProtocol::classify("2023-01-01");
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_PROTOCOL_VERSION);
        assert!(peer.unknown);
    }

    #[test]
    fn discover_prefers_modern_when_listed() {
        let peer = PeerProtocol::from_discover_supported(&[
            MCP_LEGACY_LATEST_PROTOCOL_VERSION.to_string(),
            MCP_MODERN_PROTOCOL_VERSION.to_string(),
        ])
        .expect("non-empty");
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert!(!peer.unknown);
    }

    #[test]
    fn discover_empty_supported_is_none() {
        assert!(PeerProtocol::from_discover_supported(&[]).is_none());
    }

    #[test]
    fn discover_unknown_only_list_is_still_modern() {
        let peer =
            PeerProtocol::from_discover_supported(&["2027-06-01".into()]).expect("non-empty");
        assert_eq!(peer.era, PeerEra::Modern);
        assert_eq!(peer.version, MCP_MODERN_PROTOCOL_VERSION);
        assert!(peer.unknown);
    }

    #[test]
    fn initialize_omitted_version_defaults_to_client_pin() {
        let peer = PeerProtocol::from_initialize_version(None);
        assert_eq!(peer, PeerProtocol::legacy_default());
    }

    #[test]
    fn initialize_modern_date_stays_legacy_era() {
        let peer = PeerProtocol::from_initialize_version(Some(MCP_MODERN_PROTOCOL_VERSION));
        assert_eq!(peer.era, PeerEra::Legacy);
        assert_eq!(peer.version, MCP_LEGACY_LATEST_PROTOCOL_VERSION);
        assert!(peer.unknown);
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
}
