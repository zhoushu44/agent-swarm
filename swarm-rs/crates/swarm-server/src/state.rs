//! Shared server state.

use swarm_store::Store;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// When set, workers may only create items that attach under an existing
    /// parent (a split) — never new top-level work. The hard counterpart of the
    /// `--no-create` prompt rule: the prompt is advisory, this is not.
    pub forbid_item_creation: bool,
    /// Bind address.
    pub bind: String,
    /// Port.
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            forbid_item_creation: false,
            bind: "127.0.0.1".to_string(),
            port: 17771,
        }
    }
}

/// Everything the handlers need.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: ServerConfig,
}

impl AppState {
    pub fn new(store: Store, config: ServerConfig) -> Self {
        Self { store, config }
    }

    /// Whether a create request may pass under `forbid_item_creation`.
    ///
    /// Allowed: the seeder (no peer), auto-split (peer `auto-split`), and any
    /// request attaching under an existing parent — that is a split, not new
    /// top-level work.
    pub fn item_creation_allowed(&self, peer: &str, parent_id: Option<&str>) -> bool {
        if !self.config.forbid_item_creation {
            return true;
        }
        let peer = peer.trim();
        if peer.is_empty() || peer == "auto-split" {
            return true;
        }
        parent_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_is_open_by_default() {
        let st = AppState::new(Store::open_memory().unwrap(), ServerConfig::default());
        assert!(st.item_creation_allowed("w1", None));
    }

    #[test]
    fn no_create_blocks_new_top_level_but_allows_splits() {
        let st = AppState::new(
            Store::open_memory().unwrap(),
            ServerConfig {
                forbid_item_creation: true,
                ..Default::default()
            },
        );
        // A worker inventing new top-level work is refused.
        assert!(!st.item_creation_allowed("w1", None));
        // A split (parent given) is allowed.
        assert!(st.item_creation_allowed("w1", Some("i1")));
        // The seeder and auto-split are always allowed.
        assert!(st.item_creation_allowed("", None));
        assert!(st.item_creation_allowed("auto-split", None));
    }
}
