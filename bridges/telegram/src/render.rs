//! Pure rendering rules: message splitting, edit throttling and approval
//! button data.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use zeroclaw_gateway_client::Decision;

/// Least time between two edits of a streaming message; Telegram rate
/// limits edits.
pub const EDIT_INTERVAL: Duration = Duration::from_secs(1);

/// Telegram's limit on `callback_data`, in bytes.
pub const CALLBACK_DATA_LIMIT: usize = 64;

/// Split `text` into pieces of at most `limit` UTF-16 code units (how
/// Telegram counts), breaking after a newline in the second half of a
/// piece when there is one. Blank text gives no pieces.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.trim().is_empty() {
        let mut units = 0;
        let mut end = rest.len();
        let mut newline = None;
        for (index, ch) in rest.char_indices() {
            if units + ch.len_utf16() > limit {
                end = index;
                break;
            }
            units += ch.len_utf16();
            if ch == '\n' && units > limit / 2 {
                newline = Some(index + 1);
            }
        }
        if end < rest.len()
            && let Some(cut) = newline
        {
            end = cut;
        }
        if end == 0 {
            // A limit below one character still makes progress.
            end = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        pieces.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    pieces
}

/// Whether a streaming message may be edited now.
pub fn edit_due(last_edit: Option<Instant>, now: Instant) -> bool {
    last_edit.is_none_or(|last| now.duration_since(last) >= EDIT_INTERVAL)
}

fn decision_code(decision: Decision) -> char {
    match decision {
        Decision::Approve => 'y',
        Decision::Always => 'a',
        Decision::Deny => 'n',
    }
}

/// `ap:<key>:<y|a|n>`, or `None` when it would not fit Telegram's limit.
pub fn encode_callback(key: &str, decision: Decision) -> Option<String> {
    let data = format!("ap:{key}:{}", decision_code(decision));
    (data.len() <= CALLBACK_DATA_LIMIT).then_some(data)
}

/// The key and decision in an approval button's data.
pub fn decode_callback(data: &str) -> Option<(&str, Decision)> {
    let (key, code) = data.strip_prefix("ap:")?.rsplit_once(':')?;
    let decision = match code {
        "y" => Decision::Approve,
        "a" => Decision::Always,
        "n" => Decision::Deny,
        _ => return None,
    };
    (!key.is_empty()).then_some((key, decision))
}

/// Maps approval request ids to button keys. An id short enough is its own
/// key; a longer one gets a local `#<n>` key, and the newest
/// [`ApprovalKeys::CAPACITY`] of those are remembered.
#[derive(Debug, Default)]
pub struct ApprovalKeys {
    next: u64,
    long_ids: BTreeMap<u64, String>,
}

impl ApprovalKeys {
    pub const CAPACITY: usize = 256;

    pub fn key_for(&mut self, request_id: &str) -> String {
        let fits = encode_callback(request_id, Decision::Always).is_some();
        if fits && !request_id.starts_with('#') {
            return request_id.to_string();
        }
        self.next += 1;
        self.long_ids.insert(self.next, request_id.to_string());
        while self.long_ids.len() > Self::CAPACITY {
            self.long_ids.pop_first();
        }
        format!("#{}", self.next)
    }

    /// The request id behind a key, or `None` for a forgotten local key.
    pub fn resolve(&mut self, key: &str) -> Option<String> {
        match key.strip_prefix('#') {
            Some(n) => self.long_ids.remove(&n.parse().ok()?),
            None => Some(key.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_piece_and_blank_text_none() {
        assert_eq!(split_message("hello", 4096), ["hello"]);
        assert!(split_message("  \n ", 4096).is_empty());
        assert!(split_message("", 4096).is_empty());
    }

    #[test]
    fn long_text_splits_at_the_limit_or_a_late_newline() {
        let text = "a".repeat(10);
        assert_eq!(split_message(&text, 4), ["aaaa", "aaaa", "aa"]);
        // A newline past the half-way mark ends the piece.
        assert_eq!(split_message("abc\ndefgh", 5), ["abc\n", "defgh"]);
        // An early newline is not worth a short piece.
        assert_eq!(split_message("a\nbcdefgh", 5), ["a\nbcd", "efgh"]);
        let pieces = split_message(&"x".repeat(9000), 4096);
        assert_eq!(
            pieces.iter().map(String::len).collect::<Vec<_>>(),
            [4096, 4096, 808]
        );
    }

    #[test]
    fn splitting_counts_utf16_units_and_keeps_characters_whole() {
        // Each emoji is two UTF-16 units.
        let pieces = split_message("😀😀😀", 4);
        assert_eq!(pieces, ["😀😀", "😀"]);
        assert_eq!(pieces.concat(), "😀😀😀");
    }

    #[test]
    fn edits_wait_a_second_after_the_last_one() {
        let now = Instant::now();
        assert!(edit_due(None, now));
        assert!(!edit_due(Some(now), now + Duration::from_millis(400)));
        assert!(edit_due(Some(now), now + EDIT_INTERVAL));
    }

    #[test]
    fn callback_data_round_trips() {
        let id = "0b5f3c2e-8d6a-4f11-9a0e-2c7d9b1e4a55";
        for decision in [Decision::Approve, Decision::Always, Decision::Deny] {
            let data = encode_callback(id, decision).unwrap();
            assert!(data.len() <= CALLBACK_DATA_LIMIT);
            assert_eq!(decode_callback(&data), Some((id, decision)));
        }
        assert_eq!(decode_callback("ap:a:b:n"), Some(("a:b", Decision::Deny)));
        assert_eq!(decode_callback("ap:r1:x"), None);
        assert_eq!(decode_callback("ap::y"), None);
        assert_eq!(decode_callback("other"), None);
        assert!(encode_callback(&"z".repeat(60), Decision::Deny).is_none());
    }

    #[test]
    fn long_request_ids_get_local_keys() {
        let mut keys = ApprovalKeys::default();
        assert_eq!(keys.key_for("r1"), "r1");
        assert_eq!(keys.resolve("r1").as_deref(), Some("r1"));

        let long = "z".repeat(100);
        let key = keys.key_for(&long);
        assert_eq!(key, "#1");
        assert!(encode_callback(&key, Decision::Approve).is_some());
        assert_eq!(keys.resolve(&key).as_deref(), Some(long.as_str()));
        assert_eq!(keys.resolve(&key), None, "a local key resolves once");
        assert_eq!(keys.key_for("#7"), "#2", "ids that look local are mapped");

        for n in 0..ApprovalKeys::CAPACITY + 5 {
            keys.key_for(&format!("{long}{n}"));
        }
        assert_eq!(keys.long_ids.len(), ApprovalKeys::CAPACITY);
    }
}
