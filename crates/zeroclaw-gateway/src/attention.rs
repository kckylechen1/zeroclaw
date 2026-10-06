//! Live attention evaluation. UTC instants map to local wall time, so both
//! occurrences of a folded hour are quiet and skipped spring times need no guess.
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveTime, Utc};
use zeroclaw_config::attention::AttentionConfig;
use zeroclaw_runtime::cron::Tz;

pub(crate) fn permits(
    policy: Option<&AttentionConfig>,
    bridge: &str,
    recipient: &str,
    source_kind: &str,
    source_id: &str,
    now: DateTime<Utc>,
) -> Result<bool> {
    let Some(policy) = policy else {
        return Ok(true);
    };
    let timezone: Tz = policy
        .timezone
        .parse()
        .context("attention_invalid_timezone")?;
    let start = NaiveTime::parse_from_str(&policy.quiet_start, "%H:%M")
        .context("attention_invalid_quiet_start")?;
    let end = NaiveTime::parse_from_str(&policy.quiet_end, "%H:%M")
        .context("attention_invalid_quiet_end")?;
    // Validate the complete policy before permitting even an authorized bypass.
    if policy.important_sources.iter().any(|source| {
        source.bridge == bridge
            && source.recipient == recipient
            && source.source_kind == source_kind
            && source.source_id == source_id
    }) {
        return Ok(true);
    }
    let local = now.with_timezone(&timezone).time();
    let quiet = if start < end {
        local >= start && local < end
    } else {
        local >= start || local < end
    };
    Ok(!quiet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::attention::ImportantSource;
    #[test]
    fn dst_fold_gap_and_exact_owner_bypass() {
        let mut policy = AttentionConfig {
            timezone: "America/New_York".into(),
            quiet_start: "01:00".into(),
            quiet_end: "03:00".into(),
            ..Default::default()
        };
        let allowed = |p: &AttentionConfig, instant: &str| {
            permits(
                Some(p),
                "tg",
                "owner",
                "cron",
                "job",
                instant.parse().unwrap(),
            )
            .unwrap()
        };
        assert!(!allowed(&policy, "2026-11-01T05:30:00Z"));
        assert!(!allowed(&policy, "2026-11-01T06:30:00Z"));
        assert!(allowed(&policy, "2026-11-01T08:00:00Z"));
        assert!(!allowed(&policy, "2026-03-08T06:59:59Z"));
        assert!(allowed(&policy, "2026-03-08T07:00:00Z"));
        policy.important_sources.push(ImportantSource {
            bridge: "tg".into(),
            recipient: "other".into(),
            source_kind: "cron".into(),
            source_id: "job".into(),
        });
        assert!(!allowed(&policy, "2026-11-01T06:30:00Z"));
        policy.important_sources[0].recipient = "owner".into();
        assert!(allowed(&policy, "2026-11-01T06:30:00Z"));
        policy.timezone.clear();
        assert!(permits(Some(&policy), "tg", "owner", "cron", "job", Utc::now()).is_err());
    }
    #[test]
    fn overnight_edges_and_absent_policy_use_no_host_clock() {
        let policy = AttentionConfig {
            timezone: "Asia/Tokyo".into(),
            quiet_start: "22:00".into(),
            quiet_end: "08:00".into(),
            ..Default::default()
        };
        let permitted = |instant: &str| {
            permits(
                Some(&policy),
                "tg",
                "owner",
                "notice",
                "id",
                instant.parse().unwrap(),
            )
            .unwrap()
        };
        assert!(permitted("2026-10-06T12:59:59Z"));
        assert!(!permitted("2026-10-06T13:00:00Z"));
        assert!(!permitted("2026-10-06T22:59:59Z"));
        assert!(permitted("2026-10-06T23:00:00Z"));
        assert!(
            permits(
                None,
                "tg",
                "owner",
                "notice",
                "id",
                "2026-10-06T13:00:00Z".parse().unwrap()
            )
            .unwrap()
        );
    }
}
