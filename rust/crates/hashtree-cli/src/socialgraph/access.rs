use std::collections::HashSet;
use std::sync::Arc;

use super::{SocialGraphBackend, SocialGraphStats};

#[derive(Clone)]
pub struct SocialGraphAccessControl {
    store: Arc<dyn SocialGraphBackend>,
    max_write_distance: u32,
    allowed_pubkeys: HashSet<String>,
}

impl SocialGraphAccessControl {
    pub fn new(
        store: Arc<dyn SocialGraphBackend>,
        max_write_distance: u32,
        allowed_pubkeys: HashSet<String>,
    ) -> Self {
        Self {
            store,
            max_write_distance,
            allowed_pubkeys,
        }
    }

    pub fn check_write_access(&self, pubkey_hex: &str) -> bool {
        if self.allowed_pubkeys.contains(pubkey_hex) {
            return true;
        }

        let Ok(pk_bytes) = hex::decode(pubkey_hex) else {
            return false;
        };
        let Ok(pk) = <[u8; 32]>::try_from(pk_bytes.as_slice()) else {
            return false;
        };

        super::get_follow_distance(self.store.as_ref(), &pk)
            .map(|distance| distance <= self.max_write_distance)
            .unwrap_or(false)
    }

    pub fn stats(&self) -> SocialGraphStats {
        self.store.stats().unwrap_or_else(|_| SocialGraphStats {
            enabled: true,
            max_depth: self.max_write_distance,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_allowed_pubkey_passes() {
        let _guard = crate::socialgraph::test_lock_blocking();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let pk_hex = "aa".repeat(32);
        let mut allowed = HashSet::new();
        allowed.insert(pk_hex.clone());

        let access = SocialGraphAccessControl::new(graph_store, 1, allowed);
        assert!(access.check_write_access(&pk_hex));
    }
}
