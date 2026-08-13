//! Feature-off stand-in so production holders can type
//! `Option<Arc<CompanionStore>>` without a `tachi` cfg on every field.

/// Closed companion store. Never constructed when memcore is not linked.
#[derive(Debug)]
pub struct CompanionStore {
    _private: (),
}
