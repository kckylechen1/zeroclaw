//! Tachi delegation client configuration (`[tachi]`).
//!
//! ZeroClaw hands L2 work (ADR-017 §3) to external harnesses only through
//! Tachi's `tachi_staff` MCP tool. This section says where the Tachi daemon
//! listens, how ZeroClaw identifies itself to it, how often a delegated run
//! is polled, and which harness names the body may ask for.
//!
//! The section is closed by default (`enabled = false`). While it is closed,
//! or while the daemon is unreachable, delegation fails closed with a typed
//! "Tachi unavailable" answer; nothing is ever run locally instead.
//!
//! `endpoint` must be a loopback address. Tachi's HTTP MCP surface has no
//! caller authentication (kckylechen1/Tachi#2003), so a non-loopback
//! endpoint is refused at validation rather than merely warned about. There
//! is no override in v1.
//!
//! This section is unrelated to the `tachi` memory backend
//! (`[memory] backend = "tachi"`), which links memcore in-process and never
//! talks to the daemon.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// Default Tachi daemon MCP endpoint (loopback, Tachi's default port).
pub const DEFAULT_TACHI_ENDPOINT: &str = "http://127.0.0.1:6919/mcp";
/// Default status poll interval for a delegated run, in seconds.
pub const DEFAULT_TACHI_POLL_SECS: u64 = 15;
/// Upper bound on `poll_secs`; a slower poll would make "is it done yet"
/// answers stale enough to mislead the owner.
pub const MAX_TACHI_POLL_SECS: u64 = 3600;
/// Prefix of the derived agent identity (`zeroclaw:<agent alias>`).
pub const TACHI_AGENT_IDENTITY_PREFIX: &str = "zeroclaw:";
/// Longest agent identity Tachi accepts in `x-tachi-agent-identity`.
pub const MAX_TACHI_AGENT_IDENTITY_LEN: usize = 160;

fn default_endpoint() -> String {
    DEFAULT_TACHI_ENDPOINT.to_string()
}

fn default_poll_secs() -> u64 {
    DEFAULT_TACHI_POLL_SECS
}

/// Top-level `[tachi]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "tachi"]
pub struct TachiConfig {
    /// Allow delegation to external harnesses through Tachi. Default: `false`.
    /// When `false`, a delegation request returns a typed "Tachi unavailable"
    /// answer and nothing runs.
    pub enabled: bool,
    /// Tachi daemon MCP endpoint. Must be `http://` on a loopback host
    /// (`127.0.0.0/8`, `::1`, or `localhost`): Tachi has no caller
    /// authentication yet. Default: `http://127.0.0.1:6919/mcp`.
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Identity sent as `x-tachi-agent-identity`. Unset means
    /// `zeroclaw:<agent alias>`. Allowed characters: ASCII letters, digits,
    /// `-`, `_`, `.`, `:` (at most 160).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<String>,
    /// Tachi project name, sent as `x-tachi-project` and as the `project`
    /// hint of every start. Unset means no project binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Seconds between status polls of a delegated run. Default: `15`.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// Harness names the body may request, each mapped to a Tachi dispatch
    /// profile (for example `codex = "codex_55_review"`). A name that is not
    /// listed is refused before Tachi is contacted. Empty by default.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub harnesses: HashMap<String, String>,
}

impl Default for TachiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_endpoint(),
            agent_identity: None,
            project: None,
            poll_secs: default_poll_secs(),
            harnesses: HashMap::new(),
        }
    }
}

/// True when `value` is a Tachi-acceptable agent identity assertion
/// (mirrors Tachi's `valid_agent_identity_assertion`).
#[must_use]
pub fn is_valid_tachi_agent_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TACHI_AGENT_IDENTITY_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// True when `host` names the local machine.
fn is_loopback_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

impl TachiConfig {
    /// True when every field is at its compiled default (missing-section
    /// equivalent). Keeps an untouched section off disk.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// The identity sent as `x-tachi-agent-identity` for `agent_alias`.
    ///
    /// The configured value wins. Otherwise the identity is
    /// `zeroclaw:<alias>`, with every character Tachi would refuse replaced
    /// by `_` and the result cut to Tachi's length limit.
    #[must_use]
    pub fn resolved_agent_identity(&self, agent_alias: &str) -> String {
        if let Some(identity) = self
            .agent_identity
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return identity.to_string();
        }
        let alias: String = agent_alias
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let alias = if alias.is_empty() {
            "default".to_string()
        } else {
            alias
        };
        let mut identity = format!("{TACHI_AGENT_IDENTITY_PREFIX}{alias}");
        identity.truncate(MAX_TACHI_AGENT_IDENTITY_LEN);
        identity
    }

    /// Validate the section. Only an enabled section is checked; a closed
    /// section never reaches the network.
    ///
    /// # Errors
    /// Returns `(field path, message)` for the first invalid field.
    pub fn validate(&self) -> Result<(), (String, String)> {
        if !self.enabled {
            return Ok(());
        }
        let endpoint = self.endpoint.trim();
        let parsed = url::Url::parse(endpoint).map_err(|err| {
            (
                "tachi.endpoint".to_string(),
                format!("tachi.endpoint must be a valid URL: {err}"),
            )
        })?;
        if parsed.scheme() != "http" {
            return Err((
                "tachi.endpoint".to_string(),
                "tachi.endpoint must use http:// (the daemon serves loopback HTTP only)"
                    .to_string(),
            ));
        }
        match parsed.host() {
            Some(host) if is_loopback_host(&host) => {}
            _ => {
                return Err((
                    "tachi.endpoint".to_string(),
                    format!(
                        "tachi.endpoint `{endpoint}` is not a loopback address; Tachi has no \
                         caller authentication, so it must run on this machine \
                         (127.0.0.1, ::1, or localhost)"
                    ),
                ));
            }
        }
        if let Some(identity) = self.agent_identity.as_deref()
            && !is_valid_tachi_agent_identity(identity.trim())
        {
            return Err((
                "tachi.agent_identity".to_string(),
                "tachi.agent_identity may only use ASCII letters, digits, `-`, `_`, `.`, `:` \
                 (1 to 160 characters)"
                    .to_string(),
            ));
        }
        if let Some(project) = self.project.as_deref()
            && project.trim().is_empty()
        {
            return Err((
                "tachi.project".to_string(),
                "tachi.project must not be empty when set".to_string(),
            ));
        }
        if self.poll_secs == 0 || self.poll_secs > MAX_TACHI_POLL_SECS {
            return Err((
                "tachi.poll_secs".to_string(),
                format!("tachi.poll_secs must be between 1 and {MAX_TACHI_POLL_SECS}"),
            ));
        }
        for (name, profile) in &self.harnesses {
            if name.trim().is_empty() {
                return Err((
                    "tachi.harnesses".to_string(),
                    "tachi.harnesses must not contain an empty harness name".to_string(),
                ));
            }
            if profile.trim().is_empty() {
                return Err((
                    format!("tachi.harnesses.{name}"),
                    format!("tachi.harnesses.{name} must name a Tachi profile"),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> TachiConfig {
        TachiConfig {
            enabled: true,
            ..TachiConfig::default()
        }
    }

    #[test]
    fn defaults_are_closed_loopback_and_fifteen_second_polling() {
        let config = TachiConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.endpoint, "http://127.0.0.1:6919/mcp");
        assert_eq!(config.poll_secs, 15);
        assert!(config.harnesses.is_empty());
        assert!(config.is_unset());
        assert!(enabled().validate().is_ok());
    }

    #[test]
    fn parses_full_section_from_toml() {
        let config: TachiConfig = toml::from_str(
            r#"
            enabled = true
            endpoint = "http://localhost:7000/mcp"
            agent_identity = "zeroclaw:home"
            project = "zeroclaw"
            poll_secs = 30

            [harnesses]
            codex = "codex_55_review"
            claude = "claude_plan"
            "#,
        )
        .expect("parse");
        assert!(config.enabled);
        assert_eq!(config.endpoint, "http://localhost:7000/mcp");
        assert_eq!(config.poll_secs, 30);
        assert_eq!(config.project.as_deref(), Some("zeroclaw"));
        assert_eq!(
            config.harnesses.get("codex").map(String::as_str),
            Some("codex_55_review")
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_refused() {
        let err = toml::from_str::<TachiConfig>("cancel_cli_runs = true").unwrap_err();
        assert!(err.to_string().contains("cancel_cli_runs"), "{err}");
    }

    #[test]
    fn non_loopback_endpoint_is_refused_when_enabled() {
        for endpoint in [
            "http://192.168.1.20:6919/mcp",
            "http://tachi.example.com/mcp",
            "http://0.0.0.0:6919/mcp",
            "https://127.0.0.1:6919/mcp",
            "not a url",
        ] {
            let config = TachiConfig {
                endpoint: endpoint.to_string(),
                ..enabled()
            };
            let (path, _) = config.validate().expect_err(endpoint);
            assert_eq!(path, "tachi.endpoint", "{endpoint}");
        }
        for endpoint in [
            "http://127.0.0.1:6919/mcp",
            "http://127.1.2.3:6919/mcp",
            "http://[::1]:6919/mcp",
            "http://LOCALHOST:6919/mcp",
        ] {
            let config = TachiConfig {
                endpoint: endpoint.to_string(),
                ..enabled()
            };
            assert!(config.validate().is_ok(), "{endpoint}");
        }
        // A closed section is not checked: it never reaches the network.
        let closed = TachiConfig {
            endpoint: "http://192.168.1.20/mcp".to_string(),
            ..TachiConfig::default()
        };
        assert!(closed.validate().is_ok());
    }

    #[test]
    fn invalid_fields_name_their_path() {
        let cases = [
            (
                TachiConfig {
                    agent_identity: Some("has space".to_string()),
                    ..enabled()
                },
                "tachi.agent_identity",
            ),
            (
                TachiConfig {
                    poll_secs: 0,
                    ..enabled()
                },
                "tachi.poll_secs",
            ),
            (
                TachiConfig {
                    project: Some("  ".to_string()),
                    ..enabled()
                },
                "tachi.project",
            ),
            (
                TachiConfig {
                    harnesses: HashMap::from([("codex".to_string(), " ".to_string())]),
                    ..enabled()
                },
                "tachi.harnesses.codex",
            ),
        ];
        for (config, expected) in cases {
            let (path, _) = config.validate().expect_err(expected);
            assert_eq!(path, expected);
        }
    }

    #[test]
    fn config_root_carries_tachi_section_and_validates_it() {
        let mut cfg: crate::schema::Config = toml::from_str(
            r#"
            [tachi]
            enabled = true
            [tachi.harnesses]
            codex = "codex_55_review"
            "#,
        )
        .expect("config with [tachi] parses");
        assert!(cfg.tachi.enabled);
        assert_eq!(cfg.tachi.poll_secs, DEFAULT_TACHI_POLL_SECS);
        assert!(cfg.validate().is_ok());

        cfg.tachi.endpoint = "http://10.0.0.5:6919/mcp".to_string();
        let err = cfg.validate().expect_err("remote endpoint refused");
        assert!(err.to_string().contains("loopback"), "{err}");

        // An untouched section stays off disk.
        let default_toml = toml::to_string(&crate::schema::Config::default()).expect("serialize");
        assert!(!default_toml.contains("[tachi]"), "{default_toml}");
    }

    #[test]
    fn agent_identity_defaults_to_sanitized_alias() {
        let config = TachiConfig::default();
        assert_eq!(config.resolved_agent_identity("home"), "zeroclaw:home");
        assert_eq!(
            config.resolved_agent_identity("my agent/1"),
            "zeroclaw:my_agent_1"
        );
        assert_eq!(config.resolved_agent_identity(""), "zeroclaw:default");
        assert!(is_valid_tachi_agent_identity(
            &config.resolved_agent_identity(&"x".repeat(400))
        ));
        let explicit = TachiConfig {
            agent_identity: Some("butler.main".to_string()),
            ..TachiConfig::default()
        };
        assert_eq!(explicit.resolved_agent_identity("home"), "butler.main");
    }
}
