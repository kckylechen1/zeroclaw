//! Dual-era MCP peer classification and modern-wire helpers.
//!
//! Protocol version `2026-07-28` removed the `initialize` handshake. This
//! module is the seam that must exist *before* [`MCP_PROTOCOL_VERSION`] can
//! move: probe once per server, resolve a [`PeerEra`], and branch handshake /
//! session / transport on that one value. Stage 2 speaks the modern POST
//! headers and per-request `_meta` only on [`PeerEra::Modern`]; Stage 3
//! classifies `resultType` via [`classify_mcp_result`] the same way. Stage 4
//! mints MRTR handles in [`crate::mcp_task`] and retries through
//! [`attach_input_retry`]; the same store maps unsolicited
//! `resultType: "task"` handles for `tasks/get` / `tasks/update`. The Legacy
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
/// Official tasks extension id. Advertised only on the Modern `_meta` arm.
pub const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";

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

/// Capabilities advertised on every Modern request. Legacy initialize and
/// the era probe keep their existing (empty / resources+prompts) shapes.
pub fn modern_client_capabilities() -> serde_json::Value {
    json!({
        "resources": {},
        "prompts": {},
        "extensions": {
            TASKS_EXTENSION: {}
        }
    })
}

/// Per-request `_meta` object for a modern-era JSON-RPC call.
pub fn client_request_meta(version: &str) -> serde_json::Value {
    json!({
        META_PROTOCOL_VERSION: version,
        META_CLIENT_INFO: {
            "name": "zeroclaw",
            "version": env!("CARGO_PKG_VERSION")
        },
        META_CLIENT_CAPABILITIES: modern_client_capabilities()
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

/// Ordinary completed result (`resultType: "complete"`).
pub const RESULT_TYPE_COMPLETE: &str = "complete";
/// Multi round-trip interim result (`resultType: "input_required"`).
pub const RESULT_TYPE_INPUT_REQUIRED: &str = "input_required";
/// Tasks-extension handle (`resultType: "task"`). Legal only on Modern
/// [`method_allows_task`] methods after the client advertised the extension.
pub const RESULT_TYPE_TASK: &str = "task";
/// Server-issued `taskId` is stored opaquely. Larger values fail closed.
pub const MAX_TASK_ID_BYTES: usize = 256;
/// Bound for `statusMessage` / timestamp strings on a task envelope.
pub const MAX_TASK_STRING_BYTES: usize = 4096;

/// Methods on which a server MAY return [`McpResultKind::InputRequired`].
pub fn method_allows_input_required(method: &str) -> bool {
    matches!(method, "tools/call" | "prompts/get" | "resources/read")
}

/// Methods that may return [`McpResultKind::Task`]. The extension currently
/// names only `tools/call`; any other method is an invalid response.
pub fn method_allows_task(method: &str) -> bool {
    method == "tools/call"
}

fn is_allowed_input_request_method(method: &str) -> bool {
    matches!(
        method,
        "elicitation/create" | "sampling/createMessage" | "roots/list"
    )
}

/// Parsed `InputRequiredResult` envelope. `request_state` is opaque: callers
/// MUST echo it unchanged and MUST NOT inspect it.
#[derive(Debug, Clone, PartialEq)]
pub struct InputRequired {
    pub input_requests: Option<serde_json::Map<String, serde_json::Value>>,
    pub request_state: Option<String>,
}

/// Task status from a `CreateTaskResult` or `tasks/get` envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "working" => Some(Self::Working),
            "input_required" => Some(Self::InputRequired),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Parsed `CreateTaskResult` (`resultType: "task"`). `task_id` is the
/// server-issued identifier and must never be shown to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTask {
    pub task_id: String,
    pub status: TaskStatus,
    pub poll_interval_ms: Option<u64>,
}

/// Outcome of one `tasks/get` poll. The server `taskId` is not included:
/// callers already know it and must not echo it into model-visible text.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskPollState {
    Working { poll_interval_ms: Option<u64> },
    InputRequired(InputRequired),
    Completed(serde_json::Value),
    Failed { message: String },
    Cancelled,
}

/// How a JSON-RPC `result` should be consumed.
#[derive(Debug, Clone, PartialEq)]
pub enum McpResultKind {
    Complete,
    InputRequired(InputRequired),
    Task(CreateTask),
}

/// Well-formed `input_required` classified from a JSON-RPC `result`.
///
/// This is not a completed tool result. `tools/call` mints an in-process
/// [`crate::mcp_task::McpTaskPending`] handle and retries via
/// [`attach_input_retry`]. `prompts/get` and `resources/read` surface this
/// typed error without minting a handle.
#[derive(Debug, Clone, PartialEq)]
pub struct McpInputRequiredError {
    pub method: String,
    pub input_required: InputRequired,
}

impl std::fmt::Display for McpInputRequiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP `{}` returned resultType={RESULT_TYPE_INPUT_REQUIRED}",
            self.method
        )?;
        if self
            .input_required
            .input_requests
            .as_ref()
            .is_some_and(|map| !map.is_empty())
        {
            write!(f, " with inputRequests")?;
        }
        if self.input_required.request_state.is_some() {
            write!(f, " with requestState")?;
        }
        write!(f, "; not a completed result")
    }
}

impl std::error::Error for McpInputRequiredError {}

/// Why a modern `result` was rejected. Legacy results never produce this:
/// omitted `resultType` is `complete` and extra fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultTypeError {
    Missing,
    NotAnObject,
    InvalidType { raw: String },
    InputRequiredNotAllowed { method: String },
    InputRequiredEmpty,
    MalformedInputRequests,
    MalformedRequestState,
    TaskNotAllowed { method: String },
    MalformedTask,
    TaskIdTooLarge,
    NestedTask,
    NestedInputRequired,
}

impl std::fmt::Display for ResultTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "modern MCP result omitted required resultType (must not guess complete)"
            ),
            Self::NotAnObject => write!(f, "modern MCP result is not a JSON object"),
            Self::InvalidType { raw } => {
                write!(f, "unrecognized MCP resultType {raw}")
            }
            Self::InputRequiredNotAllowed { method } => write!(
                f,
                "resultType={RESULT_TYPE_INPUT_REQUIRED} is not allowed on `{method}`"
            ),
            Self::InputRequiredEmpty => write!(
                f,
                "resultType={RESULT_TYPE_INPUT_REQUIRED} needs inputRequests or requestState"
            ),
            Self::MalformedInputRequests => {
                write!(f, "malformed inputRequests on input_required result")
            }
            Self::MalformedRequestState => {
                write!(f, "malformed requestState on input_required result")
            }
            Self::TaskNotAllowed { method } => {
                write!(
                    f,
                    "resultType={RESULT_TYPE_TASK} is not allowed on `{method}`"
                )
            }
            Self::MalformedTask => write!(f, "malformed MCP task result"),
            Self::TaskIdTooLarge => {
                write!(f, "MCP taskId exceeds {MAX_TASK_ID_BYTES} bytes; refused")
            }
            Self::NestedTask => write!(
                f,
                "MCP task completed with nested resultType={RESULT_TYPE_TASK}; refused"
            ),
            Self::NestedInputRequired => write!(
                f,
                "MCP task completed with nested resultType={RESULT_TYPE_INPUT_REQUIRED}; refused"
            ),
        }
    }
}

impl std::error::Error for ResultTypeError {}

/// Classify a JSON-RPC `result` using [`PeerEra`] as the only dispatch source.
///
/// Legacy: omitted (or any) `resultType` is [`McpResultKind::Complete`] —
/// earlier-protocol servers do not carry the field. Modern: the field is
/// required; unknown values, malformed MRTR envelopes, and malformed task
/// envelopes fail closed.
pub fn classify_mcp_result(
    era: PeerEra,
    method: &str,
    result: &serde_json::Value,
) -> Result<McpResultKind, ResultTypeError> {
    match era {
        PeerEra::Legacy => Ok(McpResultKind::Complete),
        PeerEra::Modern => classify_modern_result(method, result),
    }
}

fn classify_modern_result(
    method: &str,
    result: &serde_json::Value,
) -> Result<McpResultKind, ResultTypeError> {
    let obj = result.as_object().ok_or(ResultTypeError::NotAnObject)?;
    match obj.get("resultType") {
        None => Err(ResultTypeError::Missing),
        Some(serde_json::Value::String(kind)) if kind == RESULT_TYPE_COMPLETE => {
            Ok(McpResultKind::Complete)
        }
        Some(serde_json::Value::String(kind)) if kind == RESULT_TYPE_INPUT_REQUIRED => {
            if !method_allows_input_required(method) {
                return Err(ResultTypeError::InputRequiredNotAllowed {
                    method: method.to_string(),
                });
            }
            parse_input_required(obj).map(McpResultKind::InputRequired)
        }
        Some(serde_json::Value::String(kind)) if kind == RESULT_TYPE_TASK => {
            if !method_allows_task(method) {
                return Err(ResultTypeError::TaskNotAllowed {
                    method: method.to_string(),
                });
            }
            parse_create_task(obj).map(McpResultKind::Task)
        }
        Some(other) => Err(invalid_result_type(other)),
    }
}

/// Bound untrusted `resultType` text the same way `isError` details are
/// scrubbed: secrets redacted, length capped. Applied at construction so
/// Display and model-visible error strings cannot echo an unbounded payload.
fn invalid_result_type(value: &serde_json::Value) -> ResultTypeError {
    let raw = match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    ResultTypeError::InvalidType {
        raw: zeroclaw_providers::sanitize_api_error(&raw),
    }
}

fn parse_input_required(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<InputRequired, ResultTypeError> {
    let input_requests = match obj.get("inputRequests") {
        None => None,
        Some(serde_json::Value::Object(map)) => {
            for value in map.values() {
                let Some(request) = value.as_object() else {
                    return Err(ResultTypeError::MalformedInputRequests);
                };
                let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                    return Err(ResultTypeError::MalformedInputRequests);
                };
                if !is_allowed_input_request_method(method) {
                    return Err(ResultTypeError::MalformedInputRequests);
                }
                if let Some(params) = request.get("params")
                    && !params.is_object()
                {
                    return Err(ResultTypeError::MalformedInputRequests);
                }
            }
            Some(map.clone())
        }
        Some(_) => return Err(ResultTypeError::MalformedInputRequests),
    };
    let request_state = match obj.get("requestState") {
        None => None,
        Some(serde_json::Value::String(state)) => Some(state.clone()),
        Some(_) => return Err(ResultTypeError::MalformedRequestState),
    };
    if input_requests
        .as_ref()
        .is_none_or(serde_json::Map::is_empty)
        && request_state.is_none()
    {
        return Err(ResultTypeError::InputRequiredEmpty);
    }
    Ok(InputRequired {
        input_requests,
        request_state,
    })
}

fn bounded_task_string(
    value: Option<&serde_json::Value>,
    required: bool,
) -> Result<Option<&str>, ResultTypeError> {
    match value {
        None if required => Err(ResultTypeError::MalformedTask),
        None => Ok(None),
        Some(serde_json::Value::String(s)) => {
            if s.is_empty() && required {
                return Err(ResultTypeError::MalformedTask);
            }
            if s.len() > MAX_TASK_STRING_BYTES {
                return Err(ResultTypeError::MalformedTask);
            }
            Ok(Some(s.as_str()))
        }
        Some(_) => Err(ResultTypeError::MalformedTask),
    }
}

fn parse_task_id(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, ResultTypeError> {
    match obj.get("taskId") {
        Some(serde_json::Value::String(id)) if !id.is_empty() => {
            if id.len() > MAX_TASK_ID_BYTES {
                return Err(ResultTypeError::TaskIdTooLarge);
            }
            Ok(id.clone())
        }
        Some(serde_json::Value::String(_)) | None => Err(ResultTypeError::MalformedTask),
        Some(_) => Err(ResultTypeError::MalformedTask),
    }
}

fn parse_task_status(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<TaskStatus, ResultTypeError> {
    match obj.get("status") {
        Some(serde_json::Value::String(raw)) => {
            if raw.len() > MAX_TASK_STRING_BYTES {
                return Err(ResultTypeError::MalformedTask);
            }
            TaskStatus::parse(raw).ok_or(ResultTypeError::MalformedTask)
        }
        _ => Err(ResultTypeError::MalformedTask),
    }
}

fn parse_ttl_ms(obj: &serde_json::Map<String, serde_json::Value>) -> Result<(), ResultTypeError> {
    match obj.get("ttlMs") {
        Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => Ok(()),
        _ => Err(ResultTypeError::MalformedTask),
    }
}

fn parse_poll_interval_ms(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<u64>, ResultTypeError> {
    match obj.get("pollIntervalMs") {
        None => Ok(None),
        Some(serde_json::Value::Number(n)) => {
            n.as_u64().map(Some).ok_or(ResultTypeError::MalformedTask)
        }
        Some(_) => Err(ResultTypeError::MalformedTask),
    }
}

fn parse_create_task(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<CreateTask, ResultTypeError> {
    let task_id = parse_task_id(obj)?;
    let status = parse_task_status(obj)?;
    bounded_task_string(obj.get("createdAt"), true)?;
    bounded_task_string(obj.get("lastUpdatedAt"), true)?;
    parse_ttl_ms(obj)?;
    let _ = bounded_task_string(obj.get("statusMessage"), false)?;
    let poll_interval_ms = parse_poll_interval_ms(obj)?;
    Ok(CreateTask {
        task_id,
        status,
        poll_interval_ms,
    })
}

/// Parse a `tasks/get` result. The RPC itself is `resultType: "complete"`;
/// status-specific fields ride beside it. `expected_task_id` must match
/// exactly; a mismatch is treated as a forged envelope.
pub fn parse_task_poll_result(
    expected_task_id: &str,
    result: &serde_json::Value,
) -> Result<TaskPollState, ResultTypeError> {
    let obj = result.as_object().ok_or(ResultTypeError::NotAnObject)?;
    match obj.get("resultType") {
        Some(serde_json::Value::String(kind)) if kind == RESULT_TYPE_COMPLETE => {}
        Some(other) => return Err(invalid_result_type(other)),
        None => return Err(ResultTypeError::Missing),
    }
    let task_id = parse_task_id(obj)?;
    if task_id != expected_task_id {
        return Err(ResultTypeError::MalformedTask);
    }
    let status = parse_task_status(obj)?;
    bounded_task_string(obj.get("createdAt"), true)?;
    bounded_task_string(obj.get("lastUpdatedAt"), true)?;
    parse_ttl_ms(obj)?;
    let _ = bounded_task_string(obj.get("statusMessage"), false)?;
    let poll_interval_ms = parse_poll_interval_ms(obj)?;
    match status {
        TaskStatus::Working => Ok(TaskPollState::Working { poll_interval_ms }),
        TaskStatus::Cancelled => Ok(TaskPollState::Cancelled),
        TaskStatus::InputRequired => {
            let input_required = parse_input_required(obj)?;
            if input_required
                .input_requests
                .as_ref()
                .is_none_or(serde_json::Map::is_empty)
            {
                return Err(ResultTypeError::InputRequiredEmpty);
            }
            Ok(TaskPollState::InputRequired(input_required))
        }
        TaskStatus::Completed => match obj.get("result") {
            Some(inner) if inner.is_object() => Ok(TaskPollState::Completed(inner.clone())),
            _ => Err(ResultTypeError::MalformedTask),
        },
        TaskStatus::Failed => {
            let message = match obj.get("error") {
                Some(serde_json::Value::Object(error)) => error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("task failed"),
                _ => return Err(ResultTypeError::MalformedTask),
            };
            Ok(TaskPollState::Failed {
                message: redact_known_task_id(
                    &zeroclaw_providers::sanitize_api_error(message),
                    expected_task_id,
                ),
            })
        }
    }
}

/// Replace one stored server `taskId` after `sanitize_api_error`. Only the
/// exact in-process value is redacted; nothing is guessed from shape.
pub fn redact_known_task_id(text: &str, task_id: &str) -> String {
    if task_id.is_empty() {
        return text.to_string();
    }
    text.replace(task_id, "[task-id]")
}

/// Attach MRTR retry fields to the original JSON-RPC params.
///
/// `request_state` is echoed verbatim and never inspected. When it is `None`,
/// the retry **must not** include `requestState`. `_meta` is left untouched;
/// [`attach_request_meta`] remains the modern-era overlay at send time.
pub fn attach_input_retry(
    params: serde_json::Value,
    input_responses: Option<&serde_json::Value>,
    request_state: Option<&str>,
) -> serde_json::Value {
    match params {
        serde_json::Value::Object(mut map) => {
            if let Some(responses) = input_responses {
                map.insert("inputResponses".to_string(), responses.clone());
            }
            match request_state {
                Some(state) => {
                    map.insert(
                        "requestState".to_string(),
                        serde_json::Value::String(state.to_string()),
                    );
                }
                None => {
                    map.remove("requestState");
                }
            }
            serde_json::Value::Object(map)
        }
        other => other,
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
            Some(&modern_client_capabilities())
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

    #[test]
    fn legacy_omitted_result_type_is_complete() {
        let kind = classify_mcp_result(PeerEra::Legacy, "tools/call", &json!({"ok": true}))
            .expect("legacy omitted");
        assert_eq!(kind, McpResultKind::Complete);
    }

    #[test]
    fn legacy_ignores_input_required_field() {
        let kind = classify_mcp_result(
            PeerEra::Legacy,
            "tools/call",
            &json!({
                "resultType": RESULT_TYPE_INPUT_REQUIRED,
                "requestState": "blob"
            }),
        )
        .expect("legacy does not branch on resultType");
        assert_eq!(kind, McpResultKind::Complete);
    }

    #[test]
    fn modern_complete_is_complete() {
        let kind = classify_mcp_result(
            PeerEra::Modern,
            "tools/list",
            &json!({"resultType": RESULT_TYPE_COMPLETE, "tools": []}),
        )
        .expect("modern complete");
        assert_eq!(kind, McpResultKind::Complete);
    }

    #[test]
    fn modern_omitted_result_type_fails_closed() {
        let err = classify_mcp_result(PeerEra::Modern, "tools/call", &json!({"ok": true}))
            .expect_err("modern must not guess complete");
        assert_eq!(err, ResultTypeError::Missing);
    }

    #[test]
    fn modern_non_object_result_fails_closed() {
        assert_eq!(
            classify_mcp_result(PeerEra::Modern, "tools/call", &json!("not-an-object"))
                .expect_err("non-object"),
            ResultTypeError::NotAnObject
        );
        assert_eq!(
            classify_mcp_result(PeerEra::Modern, "tools/call", &json!(null)).expect_err("null"),
            ResultTypeError::NotAnObject
        );
    }

    #[test]
    fn modern_unknown_and_non_string_result_type_fail_closed() {
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "tools/call",
                &json!({"resultType": "stream"})
            )
            .expect_err("unknown"),
            ResultTypeError::InvalidType {
                raw: "stream".into()
            }
        );
        let err = classify_mcp_result(
            PeerEra::Modern,
            "tools/call",
            &json!({"resultType": ["complete"]}),
        )
        .expect_err("array");
        assert!(matches!(err, ResultTypeError::InvalidType { .. }));
        let err = classify_mcp_result(PeerEra::Modern, "tools/call", &json!({"resultType": 1}))
            .expect_err("number");
        assert!(matches!(err, ResultTypeError::InvalidType { .. }));
    }

    #[test]
    fn modern_input_required_on_list_is_rejected() {
        let err = classify_mcp_result(
            PeerEra::Modern,
            "tools/list",
            &json!({
                "resultType": RESULT_TYPE_INPUT_REQUIRED,
                "requestState": "blob"
            }),
        )
        .expect_err("list methods cannot MRTR");
        assert_eq!(
            err,
            ResultTypeError::InputRequiredNotAllowed {
                method: "tools/list".into()
            }
        );
    }

    #[test]
    fn modern_input_required_parses_requests_and_opaque_state() {
        let kind = classify_mcp_result(
            PeerEra::Modern,
            "tools/call",
            &json!({
                "resultType": RESULT_TYPE_INPUT_REQUIRED,
                "inputRequests": {
                    "github_login": {
                        "method": "elicitation/create",
                        "params": {"mode": "form", "message": "name"}
                    }
                },
                "requestState": "AEAD-protected blob"
            }),
        )
        .expect("well-formed MRTR");
        match kind {
            McpResultKind::InputRequired(req) => {
                assert_eq!(req.request_state.as_deref(), Some("AEAD-protected blob"));
                let requests = req.input_requests.expect("requests");
                assert_eq!(
                    requests["github_login"]["method"].as_str(),
                    Some("elicitation/create")
                );
            }
            other => panic!("expected input_required, got {other:?}"),
        }
    }

    #[test]
    fn modern_input_required_request_state_only_is_valid() {
        let kind = classify_mcp_result(
            PeerEra::Modern,
            "resources/read",
            &json!({
                "resultType": RESULT_TYPE_INPUT_REQUIRED,
                "requestState": "load-shed"
            }),
        )
        .expect("requestState-only");
        match kind {
            McpResultKind::InputRequired(req) => {
                assert!(req.input_requests.is_none());
                assert_eq!(req.request_state.as_deref(), Some("load-shed"));
            }
            other => panic!("expected input_required, got {other:?}"),
        }
    }

    #[test]
    fn modern_input_required_empty_fails_closed() {
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "tools/call",
                &json!({"resultType": RESULT_TYPE_INPUT_REQUIRED})
            )
            .expect_err("empty"),
            ResultTypeError::InputRequiredEmpty
        );
    }

    #[test]
    fn modern_malformed_mrtr_fields_fail_closed_without_panic() {
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "tools/call",
                &json!({
                    "resultType": RESULT_TYPE_INPUT_REQUIRED,
                    "inputRequests": ["not", "an", "object"]
                })
            )
            .expect_err("array requests"),
            ResultTypeError::MalformedInputRequests
        );
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "prompts/get",
                &json!({
                    "resultType": RESULT_TYPE_INPUT_REQUIRED,
                    "inputRequests": {
                        "x": {"method": "tools/call"}
                    }
                })
            )
            .expect_err("wrong inner method"),
            ResultTypeError::MalformedInputRequests
        );
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "tools/call",
                &json!({
                    "resultType": RESULT_TYPE_INPUT_REQUIRED,
                    "requestState": {"not": "a string"}
                })
            )
            .expect_err("object state"),
            ResultTypeError::MalformedRequestState
        );
        let huge = "x".repeat(64 * 1024);
        let kind = classify_mcp_result(
            PeerEra::Modern,
            "tools/call",
            &json!({
                "resultType": RESULT_TYPE_INPUT_REQUIRED,
                "requestState": huge
            }),
        )
        .expect("large opaque blob is not inspected");
        match kind {
            McpResultKind::InputRequired(req) => {
                assert_eq!(req.request_state.as_ref().map(String::len), Some(64 * 1024));
            }
            other => panic!("expected input_required, got {other:?}"),
        }
    }

    #[test]
    fn modern_oversized_string_result_type_is_length_bounded() {
        let huge = "A".repeat(5000);
        let err = classify_mcp_result(PeerEra::Modern, "tools/call", &json!({"resultType": huge}))
            .expect_err("unknown huge string");
        let msg = err.to_string();
        assert!(msg.contains("unrecognized MCP resultType"), "got: {msg}");
        assert!(
            msg.contains("..."),
            "bounded detail should be truncated: {msg}"
        );
        assert!(
            msg.len() < 1000,
            "5000-char resultType not bounded: len={}",
            msg.len()
        );
        assert!(
            !msg.contains(&"A".repeat(600)),
            "unbounded payload leaked into error"
        );
    }

    #[test]
    fn modern_oversized_object_result_type_is_length_bounded() {
        let err = classify_mcp_result(
            PeerEra::Modern,
            "tools/call",
            &json!({"resultType": {"blob": "B".repeat(5000)}}),
        )
        .expect_err("object resultType");
        let msg = err.to_string();
        assert!(msg.contains("unrecognized MCP resultType"), "got: {msg}");
        assert!(
            msg.contains("..."),
            "bounded detail should be truncated: {msg}"
        );
        assert!(
            msg.len() < 1000,
            "huge object resultType not bounded: len={}",
            msg.len()
        );
        assert!(
            !msg.contains(&"B".repeat(600)),
            "unbounded object payload leaked into error"
        );
    }

    fn sample_create_task(task_id: &str) -> serde_json::Value {
        json!({
            "resultType": RESULT_TYPE_TASK,
            "taskId": task_id,
            "status": "working",
            "createdAt": "2026-07-28T00:00:00Z",
            "lastUpdatedAt": "2026-07-28T00:00:01Z",
            "ttlMs": 60_000,
            "pollIntervalMs": 0
        })
    }

    #[test]
    fn modern_client_capabilities_advertise_tasks_extension() {
        let caps = modern_client_capabilities();
        assert_eq!(caps["resources"], json!({}));
        assert_eq!(caps["prompts"], json!({}));
        assert_eq!(caps["extensions"][TASKS_EXTENSION], json!({}));
    }

    #[test]
    fn modern_task_on_tools_call_parses_handle() {
        let kind = classify_mcp_result(
            PeerEra::Modern,
            "tools/call",
            &sample_create_task("786512e2-9e0d-44bd-8f29-789f320fe840"),
        )
        .expect("well-formed task");
        match kind {
            McpResultKind::Task(task) => {
                assert_eq!(task.task_id, "786512e2-9e0d-44bd-8f29-789f320fe840");
                assert_eq!(task.status, TaskStatus::Working);
                assert_eq!(task.poll_interval_ms, Some(0));
            }
            other => panic!("expected task, got {other:?}"),
        }
    }

    #[test]
    fn modern_task_on_list_is_rejected() {
        let err = classify_mcp_result(PeerEra::Modern, "tools/list", &sample_create_task("task-1"))
            .expect_err("list methods cannot return a task");
        assert_eq!(
            err,
            ResultTypeError::TaskNotAllowed {
                method: "tools/list".into()
            }
        );
    }

    #[test]
    fn modern_malformed_and_oversized_task_fail_closed() {
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "tools/call",
                &json!({"resultType": RESULT_TYPE_TASK})
            )
            .expect_err("missing fields"),
            ResultTypeError::MalformedTask
        );
        assert_eq!(
            classify_mcp_result(
                PeerEra::Modern,
                "tools/call",
                &json!({
                    "resultType": RESULT_TYPE_TASK,
                    "taskId": "ok",
                    "status": "flying",
                    "createdAt": "2026-07-28T00:00:00Z",
                    "lastUpdatedAt": "2026-07-28T00:00:01Z",
                    "ttlMs": 1
                })
            )
            .expect_err("unknown status"),
            ResultTypeError::MalformedTask
        );
        let huge = "T".repeat(MAX_TASK_ID_BYTES + 1);
        let err = classify_mcp_result(
            PeerEra::Modern,
            "tools/call",
            &json!({
                "resultType": RESULT_TYPE_TASK,
                "taskId": huge,
                "status": "working",
                "createdAt": "2026-07-28T00:00:00Z",
                "lastUpdatedAt": "2026-07-28T00:00:01Z",
                "ttlMs": 1
            }),
        )
        .expect_err("huge taskId");
        assert_eq!(err, ResultTypeError::TaskIdTooLarge);
        let msg = err.to_string();
        assert!(!msg.contains(&"T".repeat(80)), "taskId leaked: {msg}");
        assert!(msg.len() < 200, "error should be short, got {}", msg.len());
    }

    #[test]
    fn legacy_ignores_task_shaped_result() {
        let kind =
            classify_mcp_result(PeerEra::Legacy, "tools/call", &sample_create_task("forged"))
                .expect("legacy does not branch on resultType");
        assert_eq!(kind, McpResultKind::Complete);
    }

    #[test]
    fn parse_task_poll_completed_and_mismatch() {
        let completed = json!({
            "resultType": RESULT_TYPE_COMPLETE,
            "taskId": "abc",
            "status": "completed",
            "createdAt": "2026-07-28T00:00:00Z",
            "lastUpdatedAt": "2026-07-28T00:00:02Z",
            "ttlMs": 60_000,
            "result": {"resultType": RESULT_TYPE_COMPLETE, "ok": true}
        });
        match parse_task_poll_result("abc", &completed).expect("completed") {
            TaskPollState::Completed(inner) => {
                assert_eq!(inner["ok"], true);
            }
            other => panic!("expected completed, got {other:?}"),
        }
        assert_eq!(
            parse_task_poll_result("other", &completed).expect_err("mismatch"),
            ResultTypeError::MalformedTask
        );
    }

    #[test]
    fn attach_input_retry_echoes_state_and_responses() {
        let params = attach_input_retry(
            json!({"name": "echo", "arguments": {"q": 1}}),
            Some(&json!({"github_login": {"action": "accept"}})),
            Some("AEAD-protected blob"),
        );
        assert_eq!(params["name"], "echo");
        assert_eq!(params["arguments"], json!({"q": 1}));
        assert_eq!(
            params["inputResponses"],
            json!({"github_login": {"action": "accept"}})
        );
        assert_eq!(params["requestState"], "AEAD-protected blob");
        assert!(params.get("_meta").is_none());
    }

    #[test]
    fn attach_input_retry_omits_state_when_absent() {
        let params = attach_input_retry(
            json!({"name": "echo", "requestState": "stale"}),
            Some(&json!({})),
            None,
        );
        assert!(params.get("requestState").is_none());
        assert_eq!(params["inputResponses"], json!({}));
    }

    #[test]
    fn attach_input_retry_preserves_existing_meta() {
        let params = attach_input_retry(
            json!({
                "name": "echo",
                "_meta": {"keep": true}
            }),
            None,
            Some("blob"),
        );
        assert_eq!(params["_meta"], json!({"keep": true}));
        assert_eq!(params["requestState"], "blob");
        assert!(params.get("inputResponses").is_none());
    }

    #[test]
    fn parse_task_poll_rejects_nested_task_result_type() {
        let err = parse_task_poll_result(
            "abc",
            &json!({
                "resultType": RESULT_TYPE_TASK,
                "taskId": "abc",
                "status": "working",
                "createdAt": "2026-07-28T00:00:00Z",
                "lastUpdatedAt": "2026-07-28T00:00:02Z",
                "ttlMs": 60_000
            }),
        )
        .expect_err("nested task");
        assert!(matches!(err, ResultTypeError::InvalidType { .. }));
        let msg = err.to_string();
        assert!(msg.contains("unrecognized MCP resultType"), "got: {msg}");
    }

    #[test]
    fn redact_known_task_id_replaces_only_exact_value() {
        let id = "job-786512e2-9e0d-44bd-8f29-789f320fe840";
        let sanitized = zeroclaw_providers::sanitize_api_error(&format!("failed for {id}"));
        assert!(
            sanitized.contains(id),
            "fixture id must survive sanitize: {sanitized}"
        );
        let redacted = redact_known_task_id(&sanitized, id);
        assert!(!redacted.contains(id), "id leaked: {redacted}");
        assert!(redacted.contains("[task-id]"), "got: {redacted}");
        assert_eq!(
            redact_known_task_id("no identifier here", id),
            "no identifier here"
        );
        assert_eq!(redact_known_task_id("x", ""), "x");
    }

    #[test]
    fn failed_task_error_message_redacts_task_id() {
        let id = "srv-leaked-id";
        let state = parse_task_poll_result(
            id,
            &json!({
                "resultType": RESULT_TYPE_COMPLETE,
                "taskId": id,
                "status": "failed",
                "createdAt": "2026-07-28T00:00:00Z",
                "lastUpdatedAt": "2026-07-28T00:00:02Z",
                "ttlMs": 1,
                "error": {"code": -32000, "message": format!("boom {id}")}
            }),
        )
        .expect("failed");
        match state {
            TaskPollState::Failed { message } => {
                assert!(!message.contains(id), "id leaked: {message}");
                assert!(message.contains("[task-id]"), "got: {message}");
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }
}
