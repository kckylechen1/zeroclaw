//! Persona knobs: how an agent talks, as a small set of dials.
//!
//! Config persona defaults are authored. Reviewed Soul Voice heads may
//! override them per key after owner approval. This module owns the closed
//! vocabulary and presentation guidance; nothing here widens authority.
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

    /// Distance on the closed five-level scale, used only to bound reviewed changes.
    pub fn steps_from(self, other: Self) -> u8 {
        (self as u8).abs_diff(other as u8)
    }
}

/// The dials themselves. Every field defaults to `medium`, so an agent with no
/// persona configured uses the repository-owned medium guidance.
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

/// Hard byte ceiling for repository-owned Voice guidance (ADR-014/015).
pub const VOICE_SECTION_MAX_BYTES: usize = 1024;

impl PersonaKnobs {
    pub fn level(&self, key: &str) -> Option<PersonaLevel> {
        match key {
            "warmth" => Some(self.warmth),
            "directness" => Some(self.directness),
            "explanation_density" => Some(self.explanation_density),
            "challenge" => Some(self.challenge),
            "humor" => Some(self.humor),
            _ => None,
        }
    }

    /// Render all five dials, including medium. Examples guide presentation,
    /// never permissions, evidence standards or the honesty floor.
    #[must_use]
    pub fn to_prompt_section(&self) -> Option<String> {
        let mut out = String::from("## Voice\n\n");
        for (key, level, guidance) in [
            ("warmth", self.warmth, warmth_line(self.warmth)),
            (
                "directness",
                self.directness,
                directness_line(self.directness),
            ),
            (
                "explanation_density",
                self.explanation_density,
                explanation_density_line(self.explanation_density),
            ),
            ("challenge", self.challenge, challenge_line(self.challenge)),
            ("humor", self.humor, humor_line(self.humor)),
        ] {
            let line = format!("- {key} ({}): {guidance}\n", level.as_str());
            // Fixed strings fit together; preserve whole lines if edited later.
            if out.len() + line.len() <= VOICE_SECTION_MAX_BYTES {
                out.push_str(&line);
            }
        }
        Some(out)
    }
}

fn warmth_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Stay clinical; omit social framing. Example: \"The check passed.\""
        }
        PersonaLevel::Low => "Be courteous and restrained. Example: \"Thanks. The check passed.\"",
        PersonaLevel::Medium => {
            "Be friendly without assuming intimacy. Example: \"Thanks for checking; it passed.\""
        }
        PersonaLevel::High => {
            "Acknowledge the person with warmth. Example: \"Glad we checked this together.\""
        }
        PersonaLevel::Xhigh => {
            "Be openly caring without claiming feelings or intimacy. Example: \"That sounded hard; we can take it step by step.\""
        }
    }
}

fn directness_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Offer an observation and a clear question. Example: \"The check failed. Shall we inspect the input?\""
        }
        PersonaLevel::Low => {
            "Suggest a conclusion without commanding. Example: \"I suggest checking the input first.\""
        }
        PersonaLevel::Medium => {
            "State the answer, then a useful next step. Example: \"It failed; check the input next.\""
        }
        PersonaLevel::High => {
            "Lead with the conclusion and key reason. Example: \"Fix the input: its date is invalid.\""
        }
        PersonaLevel::Xhigh => {
            "Give the verdict immediately and plainly. Example: \"The date is invalid. Correct it first.\""
        }
    }
}

fn explanation_density_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Give the answer; retain essential caveats. Example: \"It passed; live behavior is untested.\""
        }
        PersonaLevel::Low => {
            "Add one short supporting reason. Example: \"It passed because the input now validates.\""
        }
        PersonaLevel::Medium => {
            "Give the main reason and relevant limit. Example: \"The unit test passed; deployment is untested.\""
        }
        PersonaLevel::High => {
            "Summarize key evidence and alternatives. Example: \"The input check passed; the network path remains untested.\""
        }
        PersonaLevel::Xhigh => {
            "Explain the evidence and assumptions, not private reasoning. Example: \"Both cases pass; this assumes the documented input format.\""
        }
    }
}

fn challenge_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Avoid debate. Still correct factual errors and flag safety risks. Example: \"That figure is incorrect; the total is 12.\""
        }
        PersonaLevel::Low => {
            "Object when consequential. Still correct factual errors and flag safety risks. Example: \"That assumption could change the result.\""
        }
        PersonaLevel::Medium => {
            "Question unsupported assumptions respectfully. Example: \"What evidence supports that estimate?\""
        }
        PersonaLevel::High => {
            "Say the unwelcome thing when evidence warrants it. Example: \"I disagree: the test contradicts that claim.\""
        }
        PersonaLevel::Xhigh => {
            "Push back hard on weak reasoning. Never agree to be agreeable. Example: \"That conclusion is unsupported; the evidence says otherwise.\""
        }
    }
}

fn humor_line(level: PersonaLevel) -> &'static str {
    match level {
        PersonaLevel::Minimal => {
            "Keep it strictly functional; no jokes. Example: \"The build passed.\""
        }
        PersonaLevel::Low => {
            "Use levity only when clearly welcome. Example: \"One less failing test.\""
        }
        PersonaLevel::Medium => {
            "Allow occasional light humor when appropriate. Example: \"The build finally cooperated.\""
        }
        PersonaLevel::High => {
            "Use natural wit without obscuring the answer. Example: \"The bug has retired; the regression test stays.\""
        }
        PersonaLevel::Xhigh => {
            "Be playful when welcome; stop for distress or serious risk. Example: \"The bug left a forwarding address: the regression test.\""
        }
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

    #[test]
    fn voice_guidance_bytes_are_pinned_for_every_level() {
        let levels = [
            PersonaLevel::Minimal,
            PersonaLevel::Low,
            PersonaLevel::Medium,
            PersonaLevel::High,
            PersonaLevel::Xhigh,
        ];
        let expected = include_str!("persona_voice_golden.txt");
        let rendered = levels
            .into_iter()
            .map(|level| {
                PersonaKnobs {
                    warmth: level,
                    directness: level,
                    explanation_density: level,
                    challenge: level,
                    humor: level,
                }
                .to_prompt_section()
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rendered, expected);
    }

    #[test]
    fn voice_guidance_cap_keeps_all_five_dials_in_every_combination() {
        let levels = [
            PersonaLevel::Minimal,
            PersonaLevel::Low,
            PersonaLevel::Medium,
            PersonaLevel::High,
            PersonaLevel::Xhigh,
        ];
        for warmth in levels {
            for directness in levels {
                for explanation_density in levels {
                    for challenge in levels {
                        for humor in levels {
                            let rendered = PersonaKnobs {
                                warmth,
                                directness,
                                explanation_density,
                                challenge,
                                humor,
                            }
                            .to_prompt_section()
                            .unwrap();
                            assert!(rendered.len() <= VOICE_SECTION_MAX_BYTES);
                            assert_eq!(
                                rendered
                                    .lines()
                                    .filter(|line| line.starts_with("- "))
                                    .count(),
                                5
                            );
                        }
                    }
                }
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

    /// Turning the challenge dial down trims arguing, never honesty: the
    /// low positions still have to correct factual errors (ADR-014 policy
    /// floor, made explicit by ADR-015 §5).
    #[test]
    fn low_challenge_keeps_the_honesty_floor() {
        for level in [PersonaLevel::Minimal, PersonaLevel::Low] {
            let rendered = PersonaKnobs {
                challenge: level,
                ..PersonaKnobs::default()
            }
            .to_prompt_section()
            .unwrap();
            assert!(
                rendered.contains("Still correct factual errors and flag safety risks."),
                "{level:?}: {rendered}"
            );
        }
    }
}
