//! Subagent limits for this daemon (`[subagents]`).
//!
//! ## Scope: one daemon, whole process — not per agent, not per profile
//!
//! Everything in this section governs the single
//! `zeroclaw_coordinator::Coordinator` actor that a daemon boots
//! (`zeroclaw-runtime/src/control_plane/coordinator_host.rs`). There is one
//! such actor per process, it is the only writer of the child registry, and
//! every subagent spawned by any agent alias on this machine passes through
//! it. A limit it enforces is therefore a limit on *this process*, and the
//! honest place to spell it is a top-level section rather than something
//! hanging off an agent or a runtime profile.
//!
//! Per-agent or per-profile granularity is deliberately absent. The actor
//! counts one in-flight population and has no per-alias buckets to charge a
//! spawn against, so a `[agents.<alias>]` knob here would parse, validate,
//! serialise — and change nothing. That shape of key (configurable-looking,
//! inert in force) is exactly what this section exists to avoid; adding
//! per-agent limits means first giving the coordinator per-agent accounting,
//! not first giving the config file a field.
//!
//! ## Why this is its own module
//!
//! Repo rule (`AGENTS.md`, "Where New Types Go", 3): new config sections get
//! their own module here instead of another block in `schema.rs`, which
//! upstream grows by roughly 6k lines a month. `schema.rs` carries exactly one
//! line of this section — the field that hangs it onto `Config`.

use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// How many background subagent children this daemon runs at once.
///
/// The value the operator sets here is the one the coordinator enforces:
/// [`super::schema::Config::subagents`] is read at boot by
/// `control_plane::coordinator_host::start` and copied into
/// `zeroclaw_coordinator::CoordinatorConfig::max_concurrent_children`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "subagents"]
#[serde(default, deny_unknown_fields)]
pub struct SubagentsConfig {
    /// How many subagent children may be pending or active at the same time,
    /// across the whole daemon.
    ///
    /// Each one is a full agent turn: its own model calls, its own shell and
    /// tool access, its own token spend. Six is a working ceiling for one
    /// machine — high enough that ordinary fan-out never notices it, low
    /// enough that a runaway spawner is stopped while a person can still read
    /// the log.
    ///
    /// **Why not 128.** The coordinator's compiled-in default used to be 128,
    /// inherited verbatim from `DelegateTool::MAX_CONCURRENT_BACKGROUND_DELEGATIONS`
    /// (`zeroclaw-runtime/src/tools/delegate.rs`). That constant is a *runaway
    /// backstop* — "if we got here, something is broken" — and it was copied
    /// into the slot where an operating limit belongs. 128 concurrent agent
    /// turns on one machine is not a limit; it is the absence of one.
    ///
    /// **`0` disables the limit entirely**, matching the convention
    /// `zeroclaw_coordinator::at_child_capacity` and
    /// `DelegateTool::at_background_capacity` already use for their own caps.
    /// It is spelled `0` rather than omitting the key because an absent key
    /// means "use the default", and "no limit" must be something an operator
    /// has to type.
    #[serde(default = "default_max_concurrent_children")]
    pub max_concurrent_children: usize,
}

/// The daemon-wide default: six concurrent background children.
///
/// Paired by hand with `zeroclaw_coordinator::CoordinatorConfig::default()`,
/// which carries the same number for hosts that build the actor without a
/// config file. `zeroclaw-coordinator` does not depend on this crate (it is a
/// leaf on purpose), so the two literals cannot be one constant — change one,
/// change the other.
pub const DEFAULT_MAX_CONCURRENT_CHILDREN: usize = 6;

fn default_max_concurrent_children() -> usize {
    DEFAULT_MAX_CONCURRENT_CHILDREN
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            max_concurrent_children: default_max_concurrent_children(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absent `[subagents]` section, and an absent key inside a present
    /// one, both land on 6 — not on the 128 backstop this section replaced.
    #[test]
    fn absent_subagent_config_defaults_to_six_children() {
        assert_eq!(
            SubagentsConfig::default().max_concurrent_children,
            6,
            "the daemon default must be an operating limit, not delegate's runaway backstop"
        );
        let empty: SubagentsConfig = toml::from_str("").expect("an empty section must parse");
        assert_eq!(empty.max_concurrent_children, 6);
    }

    /// The key is readable from TOML, including the `0` = disabled spelling.
    #[test]
    fn max_concurrent_children_round_trips_through_toml() {
        let parsed: SubagentsConfig =
            toml::from_str("max_concurrent_children = 2").expect("must parse");
        assert_eq!(parsed.max_concurrent_children, 2);

        let disabled: SubagentsConfig =
            toml::from_str("max_concurrent_children = 0").expect("must parse");
        assert_eq!(
            disabled.max_concurrent_children, 0,
            "0 is the documented 'no limit' spelling and must survive parsing"
        );
    }

    /// A misspelled key is a hard error rather than a silently ignored line —
    /// a limit the operator believes they set and did not is worse than one
    /// they never touched.
    #[test]
    fn a_misspelled_key_is_refused_rather_than_ignored() {
        let err = toml::from_str::<SubagentsConfig>("max_concurrent_childrne = 2")
            .expect_err("unknown keys must not be silently dropped");
        assert!(
            err.to_string().contains("max_concurrent_childrne"),
            "the error must name the offending key, got: {err}"
        );
    }
}
