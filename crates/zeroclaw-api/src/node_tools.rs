//! Process-local bridge to the gateway-owned Node service. Never an authority store.
use crate::tool::Tool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock, Weak};

pub trait NodeTools: Send + Sync {
    /// Only explicitly allowed safe, device-qualified tools may be returned.
    fn tools(&self, allowed: &[String]) -> Vec<Box<dyn Tool>>;
}

type Bridges = HashMap<PathBuf, Weak<dyn NodeTools>>;
static BRIDGES: OnceLock<RwLock<Bridges>> = OnceLock::new();

/// The gateway retains the strong reference; a stopped gateway cannot be revived.
pub fn install(path: &Path, service: Arc<dyn NodeTools>) {
    if let Ok(mut bridges) = BRIDGES.get_or_init(Default::default).write() {
        bridges.retain(|_, service| service.strong_count() > 0);
        bridges.insert(path.to_path_buf(), Arc::downgrade(&service));
    }
}

pub fn tools(path: &Path, allowed: Option<&[String]>) -> Vec<Box<dyn Tool>> {
    let Some(allowed) = allowed else {
        return Vec::new();
    };
    let service = BRIDGES
        .get_or_init(Default::default)
        .read()
        .ok()
        .and_then(|bridges| bridges.get(path).and_then(Weak::upgrade));
    service.map_or_else(Vec::new, |service| service.tools(allowed))
}
