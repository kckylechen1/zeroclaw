#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MemoryBackendKind {
    Sqlite,
    Lucid,
    Markdown,
    None,
    /// A backend that was removed (`postgres`, `qdrant`). Selecting it is an
    /// explicit construction error, never a silent fallback.
    Retired(&'static str),
    #[cfg(feature = "tachi")]
    Tachi,
    Unknown,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MemoryBackendProfile {
    pub key: &'static str,
    pub label: &'static str,
    pub auto_save_default: bool,
    pub uses_sqlite_hygiene: bool,
    pub sqlite_based: bool,
    pub optional_dependency: bool,
}

const SQLITE_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "sqlite",
    label: "SQLite with Vector Search (recommended) — fast, hybrid search, embeddings",
    auto_save_default: true,
    uses_sqlite_hygiene: true,
    sqlite_based: true,
    optional_dependency: false,
};

const LUCID_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "lucid",
    label: "Lucid Memory bridge — sync with local lucid-memory CLI, keep SQLite fallback",
    auto_save_default: true,
    uses_sqlite_hygiene: true,
    sqlite_based: true,
    optional_dependency: true,
};

const MARKDOWN_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "markdown",
    label: "Markdown Files — simple, human-readable, no dependencies",
    auto_save_default: true,
    uses_sqlite_hygiene: false,
    sqlite_based: false,
    optional_dependency: false,
};

const NONE_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "none",
    label: "None — disable persistent memory",
    auto_save_default: false,
    uses_sqlite_hygiene: false,
    sqlite_based: false,
    optional_dependency: false,
};

#[cfg(feature = "tachi")]
const TACHI_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "tachi",
    label: "Tachi / memcore — hybrid FTS+vector recall with tier lifecycle",
    auto_save_default: true,
    uses_sqlite_hygiene: false,
    sqlite_based: false,
    optional_dependency: true,
};

const CUSTOM_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "custom",
    label: "Custom backend — extension point",
    auto_save_default: true,
    uses_sqlite_hygiene: false,
    sqlite_based: false,
    optional_dependency: false,
};

const RETIRED_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "retired",
    label: "Removed backend — select sqlite, lucid, markdown or none",
    auto_save_default: false,
    uses_sqlite_hygiene: false,
    sqlite_based: false,
    optional_dependency: false,
};

/// Backend names that were removed. Selecting one fails memory construction
/// with an explicit error instead of falling back to another backend. The
/// list lives with the config retirement tables so load-time warnings and
/// construction-time errors cannot drift.
pub use zeroclaw_config::validation_warnings::RETIRED_MEMORY_BACKENDS;

const SELECTABLE_MEMORY_BACKENDS: [MemoryBackendProfile; 4] = [
    SQLITE_PROFILE,
    LUCID_PROFILE,
    MARKDOWN_PROFILE,
    NONE_PROFILE,
];

pub fn selectable_memory_backends() -> &'static [MemoryBackendProfile] {
    &SELECTABLE_MEMORY_BACKENDS
}

pub fn default_memory_backend_key() -> &'static str {
    SQLITE_PROFILE.key
}

pub fn classify_memory_backend(backend: &str) -> MemoryBackendKind {
    match backend {
        "sqlite" => MemoryBackendKind::Sqlite,
        "lucid" => MemoryBackendKind::Lucid,
        "markdown" => MemoryBackendKind::Markdown,
        "none" => MemoryBackendKind::None,
        #[cfg(feature = "tachi")]
        "tachi" => MemoryBackendKind::Tachi,
        other => RETIRED_MEMORY_BACKENDS
            .iter()
            .find(|retired| **retired == other)
            .map_or(MemoryBackendKind::Unknown, |retired| {
                MemoryBackendKind::Retired(retired)
            }),
    }
}

pub fn memory_backend_profile(backend: &str) -> MemoryBackendProfile {
    match classify_memory_backend(backend) {
        MemoryBackendKind::Sqlite => SQLITE_PROFILE,
        MemoryBackendKind::Lucid => LUCID_PROFILE,
        MemoryBackendKind::Retired(_) => RETIRED_PROFILE,
        MemoryBackendKind::Markdown => MARKDOWN_PROFILE,
        MemoryBackendKind::None => NONE_PROFILE,
        #[cfg(feature = "tachi")]
        MemoryBackendKind::Tachi => TACHI_PROFILE,
        MemoryBackendKind::Unknown => CUSTOM_PROFILE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_backends() {
        assert_eq!(classify_memory_backend("sqlite"), MemoryBackendKind::Sqlite);
        assert_eq!(classify_memory_backend("lucid"), MemoryBackendKind::Lucid);
        assert_eq!(
            classify_memory_backend("markdown"),
            MemoryBackendKind::Markdown
        );
        assert_eq!(classify_memory_backend("none"), MemoryBackendKind::None);
    }

    #[test]
    fn classify_unknown_backend() {
        assert_eq!(classify_memory_backend("redis"), MemoryBackendKind::Unknown);
    }

    #[test]
    fn selectable_backends_are_ordered_for_onboarding() {
        let backends = selectable_memory_backends();
        assert_eq!(backends.len(), 4);
        assert_eq!(backends[0].key, "sqlite");
        assert_eq!(backends[1].key, "lucid");
        assert_eq!(backends[2].key, "markdown");
        assert_eq!(backends[3].key, "none");
    }

    #[test]
    fn lucid_profile_is_sqlite_based_optional_backend() {
        let profile = memory_backend_profile("lucid");
        assert!(profile.sqlite_based);
        assert!(profile.optional_dependency);
        assert!(profile.uses_sqlite_hygiene);
    }

    #[test]
    fn unknown_profile_preserves_extensibility_defaults() {
        let profile = memory_backend_profile("custom-memory");
        assert_eq!(profile.key, "custom");
        assert!(profile.auto_save_default);
        assert!(!profile.uses_sqlite_hygiene);
    }

    #[test]
    fn classify_marks_removed_backends_as_retired_not_unknown() {
        // A retired name must never classify as Unknown: Unknown falls back
        // to markdown, and a removed backend must fail explicitly instead.
        for name in RETIRED_MEMORY_BACKENDS {
            assert_eq!(
                classify_memory_backend(name),
                MemoryBackendKind::Retired(name)
            );
            assert!(
                !selectable_memory_backends().iter().any(|b| b.key == *name),
                "{name} is removed and must not be offered"
            );
        }
    }

    #[test]
    fn each_known_backend_profile_carries_a_matching_key() {
        for name in ["sqlite", "lucid", "markdown", "none"] {
            assert_eq!(
                memory_backend_profile(name).key,
                name,
                "profile for {name} should carry a matching key"
            );
        }
    }

    #[test]
    fn default_backend_key_is_sqlite_and_listed_first() {
        assert_eq!(default_memory_backend_key(), "sqlite");
        assert_eq!(
            selectable_memory_backends()[0].key,
            default_memory_backend_key()
        );
    }
}
