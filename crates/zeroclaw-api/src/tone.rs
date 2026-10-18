//! Derived provenance of an effective Tone dial; no independent stored state.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToneProvenance {
    Builtin,
    Persona {
        persona: String,
        card: Option<String>,
    },
    Stored {
        revision: u64,
    },
}
