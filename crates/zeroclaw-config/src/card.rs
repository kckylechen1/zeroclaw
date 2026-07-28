//! Agent cards: one authored unit carrying who an agent is and what it may do.
//!
//! Persona and authority are currently separate config sections. That split is
//! a hazard: an agent able to edit configuration can widen its own tool access
//! without touching anything that looks like its identity, and a signature over
//! one half proves nothing about the other. A card binds them, so a single
//! integrity check covers both.
//!
//! ## Grants are a closed world
//!
//! [`CardGrants::tools`] is the entire set of tools a carded agent may reach.
//! There is no variant meaning "everything" — the way to grant a tool is to
//! name it. This is deliberate. Elsewhere in this config an absent
//! `allowed_tools` means unrestricted while an empty one means deny-all, and
//! telling those apart has already cost real bugs. On the card path the
//! distinction cannot arise: an empty grant list is an agent that may call
//! nothing, and there is no way to spell "anything".
//!
//! ## Classes exist so reach is visible
//!
//! [`GrantClass`] separates acting on this machine from acting on other
//! agents. A card granting [`GrantClass::FleetControl`] can approve, reject or
//! dispatch another agent's work — authority delegated from a person, not
//! authority the agent holds. Keeping that legible on the grant means "which
//! cards can act on the fleet" is a query rather than an audit.

use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// What kind of reach a granted tool has.
///
/// Ordered from least to most reach, so a card's maximum class can be compared
/// without mapping to numbers at the call site.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GrantClass {
    /// Observes local state and changes nothing: file reads, memory recall,
    /// market snapshots. The default, because it is the class that cannot
    /// cause harm by being wrong about it.
    #[default]
    LocalRead,
    /// Acts on this machine: writes files, speaks, sets timers, runs commands.
    LocalAct,
    /// Observes other agents: lists runs, inspects a task, fetches an
    /// artifact. Reads across a trust boundary but changes nothing there.
    FleetRead,
    /// Acts on other agents: approves, rejects, dispatches, sends. This is
    /// delegated authority — it belongs to the person the agent serves, and an
    /// agent holding this grant is exercising it on their behalf.
    FleetControl,
}

impl GrantClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalRead => "local_read",
            Self::LocalAct => "local_act",
            Self::FleetRead => "fleet_read",
            Self::FleetControl => "fleet_control",
        }
    }

    /// Whether this class reaches beyond the local machine into other agents'
    /// work.
    #[must_use]
    pub fn crosses_trust_boundary(self) -> bool {
        matches!(self, Self::FleetRead | Self::FleetControl)
    }

    /// Whether this class can change something outside this process.
    #[must_use]
    pub fn is_mutating(self) -> bool {
        matches!(self, Self::LocalAct | Self::FleetControl)
    }
}

/// One tool a card grants, and the reach it carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[prefix = "card_grant"]
pub struct ToolGrant {
    /// Exact tool name, as the dispatcher sees it. MCP tools carry their
    /// `<server>__<tool>` form here, in full.
    pub tool: String,
    /// The reach this tool has. Stated by the card's author rather than
    /// inferred, so a tool that quietly gains reach does not silently keep an
    /// old classification.
    #[serde(default)]
    pub class: GrantClass,
}

impl ToolGrant {
    pub fn new(tool: impl Into<String>, class: GrantClass) -> Self {
        Self {
            tool: tool.into(),
            class,
        }
    }
}

/// Everything a carded agent may reach. Empty means nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "card_grants"]
pub struct CardGrants {
    /// The complete set of permitted tools. There is no "all" — naming is the
    /// only way to grant.
    pub tools: Vec<ToolGrant>,
    /// MCP bundles this agent may connect to. A bundle not listed here is
    /// unreachable regardless of what its server offers.
    pub mcp_bundles: Vec<String>,
}

impl CardGrants {
    /// The tool names, in the shape `SecurityPolicy.allowed_tools` expects.
    ///
    /// Always `Some`, never `None`: `None` there means unrestricted, and a
    /// card can never mean that. An agent with no grants gets `Some(vec![])`,
    /// which denies everything.
    #[must_use]
    pub fn to_allowed_tools(&self) -> Option<Vec<String>> {
        Some(self.tools.iter().map(|g| g.tool.clone()).collect())
    }

    /// The highest reach this card hands out, or `None` when it grants
    /// nothing. Lets an operator ask "what is the worst this card can do"
    /// without reading every line of it.
    #[must_use]
    pub fn max_class(&self) -> Option<GrantClass> {
        self.tools.iter().map(|g| g.class).max()
    }

    /// Tools in a given class. Used to answer "what fleet control does this
    /// card actually grant" without re-deriving the filter each time.
    pub fn tools_in_class(&self, class: GrantClass) -> impl Iterator<Item = &ToolGrant> {
        self.tools.iter().filter(move |g| g.class == class)
    }

    /// Whether this card can act on other agents' work.
    #[must_use]
    pub fn grants_fleet_control(&self) -> bool {
        self.tools.iter().any(|g| g.class == GrantClass::FleetControl)
    }

    /// Reject a grant list that cannot mean what it says.
    ///
    /// # Errors
    /// Returns a message naming the offending entry when a tool is listed
    /// twice, or when a name is blank.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for grant in &self.tools {
            let name = grant.tool.trim();
            if name.is_empty() {
                return Err("card grants a tool with an empty name".to_string());
            }
            if !seen.insert(name) {
                return Err(format!(
                    "card grants {name:?} twice; a tool has exactly one class"
                ));
            }
        }
        Ok(())
    }
}

/// One agent's card: its voice, its reach, and the profile supplying whatever
/// authority settings are not tool grants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "card"]
pub struct AgentCard {
    /// Which `[personas.<alias>]` dial set shapes this agent's voice.
    pub persona: crate::providers::PersonaRef,
    /// Which `[risk_profiles.<alias>]` supplies the settings that are not tool
    /// grants: autonomy level, sandbox, shell command allow-list, and the
    /// `always_ask` backstop. The card owns tools; the profile owns the rest —
    /// with one exception: the named profile's own `excluded_tools` still
    /// subtracts from the card's grants (`SecurityPolicy::for_agent` replaces
    /// only `allowed_tools`; `is_tool_allowed` is `allowed && !excluded`
    /// regardless of where `allowed` came from). That is deliberate, not a
    /// leak: this profile is the card author's own choice, so its exclusions
    /// are part of the authored posture, and deny-wins fails safe.
    pub risk_profile: crate::providers::RiskProfileRef,
    /// What this agent may reach.
    #[nested]
    pub grants: CardGrants,
}

impl AgentCard {
    /// # Errors
    /// Returns a message when the grant list is malformed.
    pub fn validate(&self) -> Result<(), String> {
        self.grants.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card_with(tools: Vec<ToolGrant>) -> AgentCard {
        AgentCard {
            grants: CardGrants {
                tools,
                ..CardGrants::default()
            },
            ..AgentCard::default()
        }
    }

    /// The whole point of the closed world: a card that grants nothing denies
    /// everything, and cannot be read as "unrestricted".
    #[test]
    fn an_empty_card_denies_every_tool() {
        let grants = CardGrants::default();
        assert_eq!(
            grants.to_allowed_tools(),
            Some(vec![]),
            "empty grants must compile to deny-all, never to None"
        );
    }

    /// `None` means unrestricted downstream. No card may ever produce it —
    /// this is the bug class that cost us a fail-open allow-list.
    #[test]
    fn no_card_can_ever_compile_to_unrestricted() {
        for grants in [
            CardGrants::default(),
            CardGrants {
                tools: vec![ToolGrant::new("file_read", GrantClass::LocalRead)],
                mcp_bundles: vec!["anything".into()],
            },
        ] {
            assert!(
                grants.to_allowed_tools().is_some(),
                "a card must never compile to an unrestricted allow-list"
            );
        }
    }

    #[test]
    fn granted_tools_reach_the_allow_list_in_full() {
        let grants = CardGrants {
            tools: vec![
                ToolGrant::new("memory_recall", GrantClass::LocalRead),
                ToolGrant::new("hapi-edge__snapshot", GrantClass::LocalRead),
            ],
            ..CardGrants::default()
        };
        assert_eq!(
            grants.to_allowed_tools(),
            Some(vec![
                "memory_recall".to_string(),
                "hapi-edge__snapshot".to_string(),
            ]),
            "MCP names must survive verbatim — they are matched exactly downstream"
        );
    }

    /// An operator must be able to ask "what is the worst this card can do"
    /// without reading every entry.
    #[test]
    fn max_class_reports_the_worst_reach_granted() {
        let card = card_with(vec![
            ToolGrant::new("memory_recall", GrantClass::LocalRead),
            ToolGrant::new("tachi__approve", GrantClass::FleetControl),
            ToolGrant::new("file_write", GrantClass::LocalAct),
        ]);
        assert_eq!(card.grants.max_class(), Some(GrantClass::FleetControl));
        assert!(card.grants.grants_fleet_control());

        let read_only = card_with(vec![ToolGrant::new(
            "memory_recall",
            GrantClass::LocalRead,
        )]);
        assert_eq!(read_only.grants.max_class(), Some(GrantClass::LocalRead));
        assert!(!read_only.grants.grants_fleet_control());
    }

    #[test]
    fn a_card_granting_nothing_has_no_max_class() {
        assert_eq!(CardGrants::default().max_class(), None);
    }

    #[test]
    fn classes_are_ordered_by_reach() {
        assert!(GrantClass::LocalRead < GrantClass::LocalAct);
        assert!(GrantClass::LocalAct < GrantClass::FleetRead);
        assert!(GrantClass::FleetRead < GrantClass::FleetControl);
    }

    #[test]
    fn trust_boundary_and_mutation_are_independent_properties() {
        assert!(!GrantClass::LocalRead.crosses_trust_boundary());
        assert!(!GrantClass::LocalRead.is_mutating());

        assert!(!GrantClass::LocalAct.crosses_trust_boundary());
        assert!(GrantClass::LocalAct.is_mutating());

        // Reading another agent's work crosses a boundary without changing it.
        assert!(GrantClass::FleetRead.crosses_trust_boundary());
        assert!(!GrantClass::FleetRead.is_mutating());

        assert!(GrantClass::FleetControl.crosses_trust_boundary());
        assert!(GrantClass::FleetControl.is_mutating());
    }

    /// A tool listed twice has two classes, and nothing sensible can be done
    /// with that — so it is refused rather than resolved by ordering.
    #[test]
    fn a_tool_granted_twice_is_rejected() {
        let card = card_with(vec![
            ToolGrant::new("shell", GrantClass::LocalAct),
            ToolGrant::new("shell", GrantClass::LocalRead),
        ]);
        let err = card.validate().expect_err("duplicate must be refused");
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn a_blank_tool_name_is_rejected() {
        let card = card_with(vec![ToolGrant::new("   ", GrantClass::LocalRead)]);
        assert!(card.validate().is_err());
    }

    #[test]
    fn a_well_formed_card_validates() {
        let card = card_with(vec![
            ToolGrant::new("memory_recall", GrantClass::LocalRead),
            ToolGrant::new("tachi__list_runs", GrantClass::FleetRead),
        ]);
        assert!(card.validate().is_ok());
    }

    #[test]
    fn tools_can_be_listed_by_class() {
        let card = card_with(vec![
            ToolGrant::new("tachi__approve", GrantClass::FleetControl),
            ToolGrant::new("tachi__reject", GrantClass::FleetControl),
            ToolGrant::new("memory_recall", GrantClass::LocalRead),
        ]);
        let control: Vec<&str> = card
            .grants
            .tools_in_class(GrantClass::FleetControl)
            .map(|g| g.tool.as_str())
            .collect();
        assert_eq!(control, vec!["tachi__approve", "tachi__reject"]);
    }

    /// An unclassified grant must land in the class that cannot cause harm by
    /// being wrong, not the most permissive one.
    #[test]
    fn an_unclassified_grant_defaults_to_the_safest_class() {
        let parsed: ToolGrant =
            toml::from_str(r#"tool = "memory_recall""#).expect("class is optional");
        assert_eq!(parsed.class, GrantClass::LocalRead);
    }

    #[test]
    fn a_card_parses_from_toml() {
        let card: AgentCard = toml::from_str(
            r#"
persona = "risk_analyst"
risk_profile = "trading_readonly"

[grants]
mcp_bundles = ["hyperion_read"]
tools = [
  { tool = "memory_recall", class = "local_read" },
  { tool = "tachi__approve", class = "fleet_control" },
]
"#,
        )
        .expect("card parses");

        assert_eq!(card.persona.as_str(), "risk_analyst");
        assert_eq!(card.risk_profile.as_str(), "trading_readonly");
        assert_eq!(card.grants.mcp_bundles, vec!["hyperion_read".to_string()]);
        assert!(card.grants.grants_fleet_control());
        assert!(card.validate().is_ok());
    }

    /// A typo in a card must fail loudly at load rather than silently granting
    /// less than the author believed.
    #[test]
    fn an_unknown_card_field_is_rejected() {
        let err = toml::from_str::<AgentCard>(
            r#"
persona = "x"
allowed_tools = ["shell"]
"#,
        )
        .expect_err("unknown fields must be refused");
        assert!(
            err.to_string().contains("allowed_tools"),
            "the error must name the offending key: {err}"
        );
    }
}
