//! Dual-era MCP peer classification.
//!
//! Protocol version `2026-07-28` removed the `initialize` handshake. This
//! module is the seam that must exist *before* [`MCP_PROTOCOL_VERSION`] can
//! move: probe once per server, resolve a [`PeerEra`], and record the peer
//! version. Stage 1 keeps today's initialize handshake as the wire for
//! **every** era; modern request `_meta` / header behaviour is a follow-up.
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

/// Revisions this client can name. Stage 1 still speaks all of them over
/// the legacy initialize wire; the modern constant is classification-only.
pub const KNOWN_PROTOCOL_VERSIONS: &[&str] = &[
    MCP_PROTOCOL_VERSION,
    MCP_LEGACY_STREAMABLE_PROTOCOL_VERSION,
    MCP_LEGACY_LATEST_PROTOCOL_VERSION,
    MCP_MODERN_PROTOCOL_VERSION,
];

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
        let mut peer = Self::classify(selected);
        peer.era = PeerEra::Modern;
        Ok(peer)
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
}
