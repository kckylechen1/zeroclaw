//! Credential redaction for the rendering layer (logs, observer events, and
//! UI-facing turn events). This never runs on the data path: tool results fed
//! back to the model and signed by HMAC receipts always carry raw bytes.

use regex::Regex;
use std::sync::LazyLock;

static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(authorization|token|api[_-]?key|password|secret|user[_-]?key|bearer|credential|set[_-]?cookie|cookie)["']?\s*[:=]\s*(?:"([^"]{8,})"|'([^']{8,})'|([a-zA-Z0-9_\-\./+=]{8,}))"#).unwrap()
});

pub fn scrub_credentials(input: &str) -> String {
    SENSITIVE_KV_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            // Full mask, no value prefix: the first characters of a secret
            // are themselves secret, so nothing of the captured value may
            // survive. Only the captured value's byte span is swapped for
            // the mask; the key, its optional quote, the separator, and any
            // value quotes stay byte-for-byte as matched — reconstructing
            // them with format! historically doubled quotes in JSON-shaped
            // text. No slicing of the value means multi-byte UTF-8 needs no
            // special case.
            let whole = caps.get(0).expect("group 0 is the whole match");
            let value = caps
                .get(2)
                .or(caps.get(3))
                .or(caps.get(4))
                .expect("the value alternation always participates");
            let value_start = value.start() - whole.start();
            let value_end = value.end() - whole.start();
            let matched = whole.as_str();
            format!(
                "{}[REDACTED]{}",
                &matched[..value_start],
                &matched[value_end..]
            )
        })
        .to_string()
}

static SENSITIVE_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(authorization|token|api[_-]?key|password|passwd|secret|user[_-]?key|bearer|credential|auth|private[_-]?key|set[_-]?cookie|cookie)"#,
    )
    .unwrap()
});

/// True when a JSON object key names credential material. Single source for
/// the structured redaction walk and the approval secret predicate so the
/// two surfaces cannot drift apart.
pub fn is_sensitive_key(key: &str) -> bool {
    SENSITIVE_KEY_REGEX.is_match(key)
}

/// Structured-aware credential scrub for a JSON value bound for a human-facing
/// surface. Object entries whose key names a credential have their string value
/// redacted in place, preserving the key; every other string leaf still runs
/// through the text [`scrub_credentials`] so inline `token=...` patterns inside
/// unrelated fields are caught too. Serialize-then-scrub would corrupt key names
/// that merely contain a sensitive word (e.g. `access_token`), so this walks the
/// value instead. Same rendering-boundary contract as [`scrub_credentials`].
pub fn scrub_credentials_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let scrubbed = map
                .into_iter()
                .map(|(key, val)| {
                    if is_sensitive_key(&key) {
                        (key, redact_credential_leaf(val))
                    } else {
                        (key, scrub_credentials_value(val))
                    }
                })
                .collect();
            serde_json::Value::Object(scrubbed)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(scrub_credentials_value).collect())
        }
        serde_json::Value::String(s) => serde_json::Value::String(scrub_credentials(&s)),
        other => other,
    }
}

/// Redact a value sitting under a credential-named key. Every value — string
/// or not — collapses to the full mask with no prefix: the first characters of
/// a secret are themselves secret. Non-string values (arrays, objects,
/// numbers) are redacted wholesale — everything under a credential-named key
/// is credential material, and structural recursion would let a composite
/// shape (e.g. `api_key: ["raw-secret"]`) resurface the secret through a
/// non-sensitive child key.
fn redact_credential_leaf(_value: serde_json::Value) -> serde_json::Value {
    serde_json::Value::String("[REDACTED]".to_string())
}

/// Serialize a JSON value for a log/render surface with credentials redacted.
/// Structured walk, not the text regex: composite shapes the regex cannot see
/// (`api_key: {"nested": "secret"}`) are redacted here. Same
/// rendering-boundary contract as [`scrub_credentials_value`].
pub fn scrub_credentials_json(value: &serde_json::Value) -> String {
    scrub_credentials_value(value.clone()).to_string()
}

#[cfg(test)]
mod tests {
    use super::{scrub_credentials, scrub_credentials_value};

    #[test]
    fn scrub_credentials_value_redacts_nested_secret_and_keeps_key() {
        let input = serde_json::json!({
            "body": {"access_token": "sk-live-abcdef0123456789", "status": "ok"},
            "count": 3
        });
        let out = scrub_credentials_value(input);
        let token = out["body"]["access_token"].as_str().unwrap();
        assert_eq!(token, "[REDACTED]");
        assert!(!token.contains("abcdef0123456789"));
        assert_eq!(out["body"]["status"], "ok");
        assert_eq!(out["count"], 3);
    }

    #[test]
    fn scrub_credentials_value_redacts_composite_secret_values() {
        // A credential-named key whose value is an array/object is credential
        // material as a whole; structural walking must not resurface it.
        let input = serde_json::json!({
            "metadata": {"api_key": ["sk-live-abcdef0123456789"]},
            "auth": {"bearer": {"token": "topsecret-token-value", "kid": "2026-01"}},
            "status": "ok"
        });
        let out = scrub_credentials_value(input);
        let rendered = out.to_string();
        assert!(
            !rendered.contains("sk-live-abcdef0123456789"),
            "array-wrapped credential must be redacted: {rendered}"
        );
        assert!(
            !rendered.contains("topsecret-token-value"),
            "object-wrapped credential must be redacted: {rendered}"
        );
        assert!(rendered.contains("[REDACTED]"));
        assert_eq!(out["status"], "ok");
    }

    #[test]
    fn scrub_credentials_value_redacts_auth_passwd_private_key_variants() {
        // The key predicate is shared with the approval secret heuristic;
        // these variants must not survive anywhere on the walk.
        let input = serde_json::json!({
            "metadata": {"auth": "SECRET-SENTINEL-auth"},
            "credentials": {"passwd": "SECRET-SENTINEL-passwd"},
            "envelope": {"private_key": "SECRET-SENTINEL-privatekey"},
            "pem": {"private-key": "SECRET-SENTINEL-private-dash"},
            "ssh": {"privatekey": "SECRET-SENTINEL-privatejoined"}
        });
        let out = scrub_credentials_value(input);
        let rendered = out.to_string();
        for sentinel in [
            "SECRET-SENTINEL-auth",
            "SECRET-SENTINEL-passwd",
            "SECRET-SENTINEL-privatekey",
            "SECRET-SENTINEL-private-dash",
            "SECRET-SENTINEL-privatejoined",
        ] {
            assert!(
                !rendered.contains(sentinel),
                "{sentinel} must be redacted: {rendered}"
            );
        }
    }

    #[test]
    fn scrub_credentials_value_redacts_authorization_and_cookie_keys() {
        let input = serde_json::json!({
            "body": {
                "authorization": "Bearer sk-live-abcdef0123456789",
                "cookie": "session=deadbeefcafebabe0123",
                "set-cookie": "sid=9f8e7d6c5b4a3210feed",
                "status": "ok"
            }
        });
        let out = scrub_credentials_value(input);
        let authorization = out["body"]["authorization"].as_str().unwrap();
        assert!(authorization.contains("[REDACTED]"));
        assert!(!authorization.contains("sk-live-abcdef0123456789"));
        let cookie = out["body"]["cookie"].as_str().unwrap();
        assert!(cookie.contains("[REDACTED]"));
        assert!(!cookie.contains("deadbeefcafebabe0123"));
        let set_cookie = out["body"]["set-cookie"].as_str().unwrap();
        assert!(set_cookie.contains("[REDACTED]"));
        assert!(!set_cookie.contains("9f8e7d6c5b4a3210feed"));
        assert_eq!(out["body"]["status"], "ok");
    }

    #[test]
    fn scrub_credentials_redacts_unquoted_base64_credential_values() {
        let input = "token=QWxh+GRpbjpvcGVu/IHNlc2FtZQ== next=public";
        let scrubbed = scrub_credentials(input);

        assert_eq!(scrubbed, "token=[REDACTED] next=public");
        assert!(!scrubbed.contains("QWxh"));
        assert!(!scrubbed.contains("IHNlc2FtZQ"));
        assert!(!scrubbed.contains("=="));
    }

    #[test]
    fn scrub_credentials_redacts_quoted_base64_credential_values() {
        let input = r#"secret="QWxhZGRpbjpvcGVu/IHNlc2FtZQ==""#;
        let scrubbed = scrub_credentials(input);

        assert_eq!(scrubbed, r#"secret="[REDACTED]""#);
        assert!(!scrubbed.contains("QWxh"));
        assert!(!scrubbed.contains("IHNlc2FtZQ"));
        assert!(!scrubbed.contains("=="));
    }
}
