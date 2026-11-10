//! Test-only channel sampling for the #584 channel-retirement campaign.
//!
//! Every #584 batch removes one `[channels.<kind>]` schema together with its
//! `channel-<kind>` compatibility feature, so a test that *names* a channel as
//! its "a retained channel" / "a retired channel" sample goes red in the batch
//! that retires that sample rather than in the batch that changes the behaviour
//! it guards (`channel-git`, `wechat` and `whatsapp-web` each did this). Samples
//! here come from the two live authorities instead: the channel registry
//! (`ChannelsConfig::channels()`, `is_retired_transport`) decides retained vs
//! retired, and the root package `[features]` table decides which features exist
//! at all. Retained samples are restricted to the surfaces #584 protects (CLI,
//! Telegram, gateway webhook, the voice-duplex Node path, ACP, Email, Discord —
//! docs/book/src/channels/overview.md), so a routine retirement batch can never
//! empty them, and every consumer calls [`require_non_empty`] **before** looping
//! over a derived list: an empty list is a loud end-state failure, never a loop
//! that proves nothing. Absence assertions for already-removed channels
//! (`slack`, `matrix`, `lark`, `feishu`, ...) stay literal names: they are true
//! forever and pin the tombstones.

use std::collections::BTreeSet;
use std::path::PathBuf;

use zeroclaw_config::schema::{Config, is_retired_transport};

use crate::util::repo_root;

const END_STATE_MARKER: &str = "#584-END-STATE";

/// Which #584 end state emptied a derived list; picks the message body of
/// [`require_non_empty`].
#[derive(Clone, Copy)]
pub enum Cause {
    /// The list came from required distribution or target-policy rows.
    /// Empty samples fail instead of silently passing a walk.
    Retired,
    /// The list came from the #584-protected channel surfaces.
    Protected,
}

/// Cause: a required distribution or target-policy sample is gone.
const LAST_RETIRED_SCHEMA: &str = "the sampled distribution rows or target policy no longer \
     exist. Remove a consumer only when its behavior has no remaining owner. \
     Do not turn an empty walk into a passing assertion.";

/// Cause: no protected channel surface ships a row any more.
const PROTECTED_SURFACE: &str = "no #584-protected channel surface (CLI, Telegram, \
     gateway webhook, the voice-duplex Node path, ACP, Email, Discord) ships a channel row any \
     more. #584 forbids retiring one, so either a batch retired a protected surface — revert it \
     — or the batch that removes the last channel row of the campaign landed, and it must DELETE \
     this consumer with `PROTECTED_RETAINED_KINDS` / `channel_features` rather than leave the \
     loop to iterate nothing (AGENTS.md rule 4).";

/// Explicit failure for a derived list that came out empty: names the end state,
/// its cause, and the code that batch must delete. `#[track_caller]` reports the
/// consumer's own location, so a call site stays one line.
#[track_caller]
pub fn require_non_empty(subject: &str, cause: Cause, len: usize) {
    let cause = match cause {
        Cause::Retired => LAST_RETIRED_SCHEMA,
        Cause::Protected => PROTECTED_SURFACE,
    };
    assert!(
        len > 0,
        "{END_STATE_MARKER} {subject} is empty at {}; this is the #584 end state, not a pass: \
         {cause} Never skip, xfail or weaken this check to make it green: the batch that empties \
         the list deletes its consumer in the same commit.",
        std::panic::Location::caller(),
    );
}

/// Channel kinds #584 protects. Retained-presence samples may only come from
/// here (`cli` and `acp_server` carry no `channel-*` feature of their own but
/// stay listed so the restriction reads as the full protected set).
pub const PROTECTED_RETAINED_KINDS: &[&str] = &[
    "acp_server",
    "cli",
    "discord",
    "email",
    "telegram",
    "voice_duplex",
    "webhook",
];

fn manifest() -> PathBuf {
    repo_root().join("Cargo.toml")
}

fn cargo_toml() -> toml::Value {
    let path = manifest();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    toml::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

fn zeroclaw_metadata() -> toml::Value {
    cargo_toml()
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("zeroclaw"))
        .cloned()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()))
}

fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// One string list from `[package.metadata.zeroclaw]`, parsed straight out of
/// Cargo.toml. Tests compare production's resolved value against the *file*,
/// not against the same in-memory value production already resolved.
pub fn registry_string_list(key: &str) -> Vec<String> {
    string_list(zeroclaw_metadata().get(key))
}

/// `[package.metadata.zeroclaw.dist_target_exclusions]`, parsed straight out of
/// Cargo.toml.
pub fn registry_target_exclusions() -> std::collections::BTreeMap<String, Vec<String>> {
    zeroclaw_metadata()
        .get("dist_target_exclusions")
        .and_then(|v| v.as_table())
        .map(|table| {
            table
                .iter()
                .map(|(target, value)| (target.clone(), string_list(Some(value))))
                .collect()
        })
        .unwrap_or_default()
}

/// Every feature name in the root package `[features]` table (Cargo's own
/// resolver, same source the spec reads).
pub fn live_feature_names() -> Vec<String> {
    let meta = cargo_metadata::MetadataCommand::new()
        .manifest_path(manifest())
        .no_deps()
        .exec()
        .expect("cargo metadata for the workspace root");
    let pkg = meta
        .root_package()
        .cloned()
        .or_else(|| meta.workspace_packages().into_iter().next().cloned())
        .expect("root package");
    pkg.features.keys().cloned().collect()
}

fn canonical_kind(kind: &str) -> String {
    kind.replace('-', "_")
}

/// The registry kind a feature stands for: `channel-voice-call` ->
/// `voice_call`, `voice-wake` -> `voice_wake`.
fn feature_kind(feature: &str) -> String {
    canonical_kind(feature.strip_prefix("channel-").unwrap_or(feature))
}

/// `(kind, retired)` for every row of the live channel registry.
fn registry_channels() -> Vec<(&'static str, bool)> {
    Config::default()
        .channels
        .channels()
        .into_iter()
        .map(|info| (info.kind, info.retired))
        .collect()
}

/// Whether the registry knows this (canonical) kind, with its retired flag.
fn registry_kind(kind: &str) -> Option<bool> {
    registry_channels()
        .into_iter()
        .find(|(known, _)| canonical_kind(known) == kind)
        .map(|(_, retired)| retired)
}

/// Whether a live feature is a channel row: the `channel-*` convention plus the
/// legacy unprefixed rows the registry knows (`voice-wake`).
pub fn is_channel_feature(feature: &str) -> bool {
    let kind = feature_kind(feature);
    feature.starts_with("channel-") || registry_kind(&kind).is_some() || is_retired_transport(&kind)
}

/// Whether a kind counts as retired **for sampling**: the registry row wins,
/// with `is_retired_transport` as the fallback for kinds whose schema is already
/// gone but whose feature row may still exist. Both authorities agree today
/// (`channels()` derives `retired` from `is_retired_transport`), so the sample
/// stays on exactly the code path doctor and the matrix use.
fn kind_is_retired(kind: &str) -> bool {
    registry_kind(kind).unwrap_or_else(|| is_retired_transport(kind))
}

/// Channel rows present in the live `[features]` table.
pub fn channel_features() -> Vec<String> {
    let rows: BTreeSet<String> = live_feature_names()
        .into_iter()
        .filter(|f| is_channel_feature(f))
        .collect();
    require_non_empty("[features] channel rows", Cause::Protected, rows.len());
    rows.into_iter().collect()
}

/// Feature names a protected kind may ship under: the `channel-<kind>`
/// convention plus the legacy unprefixed spelling (`voice-wake`).
fn feature_candidates(kind: &str) -> [String; 2] {
    let kebab = kind.replace('_', "-");
    [format!("channel-{kebab}"), kebab]
}

/// The channel features of the surfaces #584 protects, one per protected kind
/// that the live `[features]` table still ships.
///
/// Built **from the protected kinds outward** (not by filtering the live rows,
/// which would silently shrink when a protected row disappears): every protected
/// kind contributes its feature when that feature exists, no protected kind may
/// be marked retired, and an empty sample is a hard failure. A batch that retires
/// a protected surface — which #584 forbids — therefore breaks every sample
/// consumer loudly instead of quietly dropping a name from the loop.
pub fn protected_retained_channel_features() -> Vec<String> {
    let live: BTreeSet<String> = channel_features().into_iter().collect();
    let mut sampled = BTreeSet::new();
    for kind in PROTECTED_RETAINED_KINDS {
        assert!(
            registry_kind(kind) != Some(true),
            "{kind} is a #584-protected surface, but the channel registry marks it retired"
        );
        for feature in feature_candidates(kind) {
            if live.contains(&feature) {
                assert!(
                    !kind_is_retired(&feature_kind(&feature)),
                    "{feature} samples a kind the registry marks retired"
                );
                sampled.insert(feature);
            }
        }
    }
    let sampled = sampled.into_iter().collect::<Vec<_>>();
    require_non_empty("protected channel sample", Cause::Protected, sampled.len());
    sampled
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positive + negative control for the protected sample: it is exactly the
    /// protected set that ships a feature, it never contains a retired kind, and
    /// it still contains the protected surfaces that must never disappear.
    /// Dropping one entry from `PROTECTED_RETAINED_KINDS` — the cheap way to
    /// silence a red sample assertion — fails the coverage comparison here.
    #[test]
    fn protected_sample_is_the_protected_set_that_ships_a_feature() {
        let live = channel_features();
        let mut expected: Vec<String> = PROTECTED_RETAINED_KINDS
            .iter()
            .copied()
            .flat_map(feature_candidates)
            .filter(|f| live.contains(f))
            .collect();
        expected.sort();
        expected.dedup();
        require_non_empty("expected sample", Cause::Protected, expected.len());

        let sampled = protected_retained_channel_features();
        assert_eq!(sampled, expected);
        for feature in &sampled {
            assert!(
                PROTECTED_RETAINED_KINDS.contains(&feature_kind(feature).as_str()),
                "{feature} is not a #584-protected surface"
            );
            assert!(
                !kind_is_retired(&feature_kind(feature)),
                "{feature} samples a kind the registry marks retired"
            );
        }
        // Never silently some other channel than the one #584 protects.
        assert!(sampled.contains(&"channel-telegram".to_string()));
        for feature in [
            "channel-acp-server",
            "channel-discord",
            "channel-email",
            "channel-telegram",
            "channel-webhook",
        ] {
            assert!(
                expected.iter().any(|s| s == feature),
                "{feature} is a #584-protected surface and must still ship"
            );
        }
    }
}
