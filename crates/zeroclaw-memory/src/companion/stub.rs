//! Feature-off stand-in so production holders can type
//! `Option<Arc<CompanionStore>>` without a `tachi` cfg on every field.

/// Closed companion store. Never constructed when memcore is not linked.
#[derive(Debug)]
pub struct CompanionStore {
    _private: (),
}

impl CompanionStore {
    /// Filesystem path of this store. Unreachable: the feature-off factory
    /// always returns `None`.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        std::path::Path::new("")
    }

    /// Directory beside the database. Unreachable: the factory returns `None`.
    #[must_use]
    pub fn store_dir(&self) -> Option<&std::path::Path> {
        None
    }
}
