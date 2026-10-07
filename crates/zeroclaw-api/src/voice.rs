//! Derived provenance of an effective Voice dial; no independent stored state.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceProvenance {
    Builtin,
    Persona {
        persona: String,
        card: Option<String>,
    },
    Stored {
        revision: u64,
    },
}
