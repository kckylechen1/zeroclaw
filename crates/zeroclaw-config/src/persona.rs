//! Persona knobs: how an agent talks, as a small set of dials.
//!
//! A persona is *authored*, not learned. It is part of an agent's definition,
//! in the same sense as its risk profile — something a person sets and the
//! agent reads. Nothing here is derived from conversation history, and nothing
//! here can widen what an agent is allowed to do.
//!
//! The dials use the same five-step vocabulary as `reasoning_effort`
//! (`minimal` / `low` / `medium` / `high` / `xhigh`) so one mental model covers
//! both. They are enums rather than free strings or floats on purpose: a
//! number invites arithmetic, and the moment a warmth value can be multiplied
//! into a score it has stopped being a delivery hint and started being an
//! input to a decision.
//!
//! ## Why these five dials
//!
//! Each earns its place from a documented failure, not from taste:
//!
//! - [`PersonaKnobs::challenge`] is the structural answer to sycophancy. An
//!   assistant tuned for agreement drifts into telling people what they want
//!   to hear, which in a trading context rebuilds the procyclical problem
//!   inside the relationship. A high challenge setting is what makes
//!   disagreement part of the contract instead of a lapse in manners.
//! - [`PersonaKnobs::directness`] exists because correct advice delivered as a
//!   command provokes resistance. Probing rather than commanding is a delivery
//!   technique, not a personality flourish.
//! - [`PersonaKnobs::explanation_density`], [`PersonaKnobs::warmth`] and
//!   [`PersonaKnobs::humor`] cover the rest of the observable temperament:
//!   how much reasoning is shown, how much heat is in the voice, and whether
//!   levity is allowed.
//!
//! What is deliberately *not* here: reminder intensity and delivery mode.
//! Those are properties of a single message — derived per turn from what is
//! happening — not properties of who the agent is.

use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// One dial position, sharing `reasoning_effort`'s vocabulary.
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
#[serde(rename_all = "lowercase")]
pub enum PersonaLevel {
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
}

impl PersonaLevel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Parse a dial position, accepting surrounding whitespace and any case —
    /// matching how `reasoning_effort` is normalized.
    ///
    /// # Errors
    /// Returns a message naming the accepted values when `value` is not one.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            other => Err(format!(
                "persona level {other:?} is invalid (expected one of: minimal, low, medium, high, xhigh)"
            )),
        }
    }

    /// Whether this dial is far enough from the middle to be worth spending
    /// prompt budget on. A dial left at `medium` says nothing the model does
    /// not already default to, so it is omitted from the rendered prompt.
    #[must_use]
    pub fn is_notable(self) -> bool {
        self != Self::Medium
    }
}

/// The dials themselves. Every field defaults to `medium`, so an agent with no
/// persona configured renders no persona text at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "persona"]
pub struct PersonaKnobs {
    /// How much heat is in the voice, from clinical to openly friendly.
    pub warmth: PersonaLevel,
    /// Probing versus stating. Low asks questions and leaves the conclusion to
    /// the reader; high states the conclusion first.
    pub directness: PersonaLevel,
    /// How much of the reasoning is shown alongside the answer.
    pub explanation_density: PersonaLevel,
    /// Willingness to disagree, push back, and say the unwelcome thing. This
    /// is the anti-sycophancy dial; turning it down is a deliberate act.
    pub challenge: PersonaLevel,
    /// Whether levity is permitted.
    pub humor: PersonaLevel,
}

impl PersonaKnobs {
    /// Render the dials as prompt text, or `None` when every dial sits at
    /// `medium` and there is nothing worth saying.
    ///
    /// Only off-centre dials are rendered. This keeps the prompt proportional
    /// to how unusual the persona actually is, rather than spending five lines
    /// restating defaults on every turn.
    #[must_use]
    pub fn to_prompt_section(&self) -> Option<String> {
        let lines: Vec<&'static str> = [
            (self.warmth, warmth_line(self.warmth)),
            (self.directness, directness_line(self.directness)),
            (
                self.explanation_density,
                explanation_density_line(self.explanation_density),
            ),
            (self.challenge, challenge_line(self.challenge)),
            (self.humor, humor_line(self.humor)),
        ]
        .into_iter()
        .filter_map(|(level, line)| level.is_notable().then_some(line))
        .collect();

        if lines.is_empty() {
            return None;
        }

        let mut out = String::from("## Voice\n\n");
        for line in lines {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
        Some(out)
    }
}

fn warmth_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => "Keep the voice flat and clinical. No pleasantries, no warmth.",
        PersonaLevel::Low => "Stay cool and businesslike. Skip the social framing.",
        PersonaLevel::Medium => "",
        PersonaLevel::High => "Be warm. It is fine to sound like you are on their side.",
        PersonaLevel::Xhigh => {
            "Be openly warm and personal. Care about the person, not just the task."
        }
    }
}

fn directness_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Never issue a conclusion. Lay out what you see and ask what they make of it."
        }
        PersonaLevel::Low => {
            "Prefer questions to statements. Offer the conclusion as one reading among others."
        }
        PersonaLevel::Medium => "",
        PersonaLevel::High => "State the conclusion first, then the reasoning behind it.",
        PersonaLevel::Xhigh => {
            "Lead with the verdict in one sentence. Do not soften it or bury it in preamble."
        }
    }
}

fn explanation_density_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => "Give the answer alone. No reasoning unless asked.",
        PersonaLevel::Low => "Give the answer with one line of justification at most.",
        PersonaLevel::Medium => "",
        PersonaLevel::High => "Show the reasoning that carries weight, and name what you checked.",
        PersonaLevel::Xhigh => {
            "Show the full derivation, including what you ruled out and why."
        }
    }
}

fn challenge_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Do not argue. Answer what was asked and leave disagreements alone."
        }
        PersonaLevel::Low => "Raise objections only when the stakes are high.",
        PersonaLevel::Medium => "",
        PersonaLevel::High => {
            "Say the unwelcome thing when it is true. Agreement is not the goal; \
             being right is."
        }
        PersonaLevel::Xhigh => {
            "Push back hard on weak reasoning, including the user's. If you think \
             they are wrong, say so plainly and say why. Never agree to be agreeable."
        }
    }
}

fn humor_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => "No humour. Keep it strictly functional.",
        PersonaLevel::Low => "Humour only when it costs nothing.",
        PersonaLevel::Medium => "",
        PersonaLevel::High => "Wit is welcome where it lands naturally.",
        PersonaLevel::Xhigh => "Be funny. A sharp joke is worth the line it costs.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_like_reasoning_effort() {
        for (raw, expected) in [
            ("minimal", PersonaLevel::Minimal),
            ("  LOW ", PersonaLevel::Low),
            ("Medium", PersonaLevel::Medium),
            ("high", PersonaLevel::High),
            ("XHIGH", PersonaLevel::Xhigh),
        ] {
            assert_eq!(PersonaLevel::parse(raw), Ok(expected), "parsing {raw:?}");
        }
    }

    #[test]
    fn an_invalid_level_names_the_accepted_values() {
        let err = PersonaLevel::parse("warm").expect_err("must reject");
        assert!(err.contains("minimal, low, medium, high, xhigh"), "{err}");
    }

    /// A persona nobody configured must not cost prompt budget.
    #[test]
    fn all_defaults_render_nothing() {
        assert_eq!(PersonaKnobs::default().to_prompt_section(), None);
    }

    /// Only the dials that were actually moved appear — the prompt stays
    /// proportional to how unusual the persona is.
    #[test]
    fn only_off_centre_dials_are_rendered() {
        let knobs = PersonaKnobs {
            challenge: PersonaLevel::Xhigh,
            ..PersonaKnobs::default()
        };
        let rendered = knobs.to_prompt_section().expect("one dial moved");

        assert!(rendered.contains("Push back hard"), "{rendered}");
        assert_eq!(
            rendered.lines().filter(|l| l.starts_with("- ")).count(),
            1,
            "medium dials must not be rendered: {rendered}"
        );
    }

    /// The anti-sycophancy dial has to actually say the thing. A persona layer
    /// that cannot express "disagree with me" has no answer to an assistant
    /// that drifts toward telling people what they want to hear.
    #[test]
    fn the_challenge_dial_can_demand_disagreement() {
        let knobs = PersonaKnobs {
            challenge: PersonaLevel::Xhigh,
            ..PersonaKnobs::default()
        };
        let rendered = knobs.to_prompt_section().expect("rendered");
        assert!(
            rendered.contains("Never agree to be agreeable"),
            "the highest challenge setting must forbid sycophancy outright: {rendered}"
        );
    }

    #[test]
    fn every_dial_renders_at_every_off_centre_position() {
        let positions = [
            PersonaLevel::Minimal,
            PersonaLevel::Low,
            PersonaLevel::High,
            PersonaLevel::Xhigh,
        ];
        for level in positions {
            for knobs in [
                PersonaKnobs {
                    warmth: level,
                    ..PersonaKnobs::default()
                },
                PersonaKnobs {
                    directness: level,
                    ..PersonaKnobs::default()
                },
                PersonaKnobs {
                    explanation_density: level,
                    ..PersonaKnobs::default()
                },
                PersonaKnobs {
                    challenge: level,
                    ..PersonaKnobs::default()
                },
                PersonaKnobs {
                    humor: level,
                    ..PersonaKnobs::default()
                },
            ] {
                let rendered = knobs
                    .to_prompt_section()
                    .unwrap_or_else(|| panic!("{level:?} must render for {knobs:?}"));
                assert!(
                    rendered.lines().filter(|l| l.starts_with("- ")).count() == 1,
                    "exactly one line expected: {rendered}"
                );
            }
        }
    }

    /// Dials are ordered, so a caller can compare positions without mapping
    /// them to numbers of its own.
    #[test]
    fn levels_are_ordered() {
        assert!(PersonaLevel::Minimal < PersonaLevel::Medium);
        assert!(PersonaLevel::Medium < PersonaLevel::Xhigh);
    }
}
