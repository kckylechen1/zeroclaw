//! SOP timestamp helpers shared by the engine, tools, and gateway surfaces.
//!
//! Extracted from `engine.rs` so cooldown / ISO-8601 utilities do not sit next
//! to run-lifecycle chokepoints. Keep the format (`YYYY-MM-DDTHH:MM:SSZ`) stable:
//! admission, park timeouts, and persisted run records all parse what we emit.

/// Current UTC time as a compact ISO-8601 string without a chrono dependency.
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // Simple UTC timestamp without chrono dependency
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y-M-D (simplified — good enough for run IDs)
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    days += 719_468;
    let era = days / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Check if enough time has elapsed since a timestamp string.
pub(crate) fn cooldown_elapsed(completed_at: &str, cooldown_secs: u64) -> bool {
    // Parse the ISO-8601 timestamp we generate
    let completed = parse_iso8601_secs(completed_at);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match completed {
        Some(ts) => now.saturating_sub(ts) >= cooldown_secs,
        None => true, // Can't parse timestamp; allow start
    }
}

/// Minimal ISO-8601 parser returning seconds since epoch.
pub(crate) fn parse_iso8601_secs(input: &str) -> Option<u64> {
    // Expected format: YYYY-MM-DDTHH:MM:SSZ
    let input = input.trim_end_matches('Z');
    let parts: Vec<&str> = input.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date_parts: Vec<u64> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u64> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();
    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }
    let (year, month, day) = (date_parts[0], date_parts[1], date_parts[2]);
    let (hour, min, sec) = (time_parts[0], time_parts[1], time_parts[2]);

    // Reverse of days_to_ymd: compute days since epoch
    let year_adj = if month <= 2 { year - 1 } else { year };
    let month_adj = if month > 2 { month - 3 } else { month + 9 };
    let era = year_adj / 400;
    let yoe = year_adj - era * 400;
    let doy = (153 * month_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso8601_round_trips_through_parser() {
        let stamped = now_iso8601();
        let parsed = parse_iso8601_secs(&stamped).expect("parser accepts emitter format");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        assert!(now.saturating_sub(parsed) <= 2);
    }

    #[test]
    fn parse_iso8601_secs_rejects_malformed() {
        assert!(parse_iso8601_secs("not-a-timestamp").is_none());
        assert!(parse_iso8601_secs("2020-01-01").is_none());
        assert!(parse_iso8601_secs("2020-01-01T00:00:00Z").is_some());
    }

    #[test]
    fn cooldown_elapsed_true_when_unparseable_or_old() {
        assert!(cooldown_elapsed("bogus", 60));
        assert!(cooldown_elapsed("1970-01-01T00:00:00Z", 1));
    }

    #[test]
    fn days_to_ymd_known_epoch_day() {
        // 0 days since epoch => 1970-01-01
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        // 1 day => 1970-01-02
        assert_eq!(days_to_ymd(1), (1970, 1, 2));
    }
}
