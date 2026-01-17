//! Blossom integration for worker operations
//!
//! Provides upload/download to Blossom servers with NIP-98 authentication.

use hashtree_blossom::{BlossomClient, BlossomError};
use nostr_sdk::Keys;
use parking_lot::RwLock;
use tracing::{debug, info};

/// Default Blossom servers
const DEFAULT_WRITE_SERVERS: &[&str] = &[
    "https://upload.iris.to",
];

const DEFAULT_READ_SERVERS: &[&str] = &[
    "https://cdn.iris.to",
];

/// Blossom manager for upload/download operations
pub struct BlossomManager {
    client: RwLock<Option<BlossomClient>>,
    keys: RwLock<Option<Keys>>,
    pending_servers: RwLock<Option<(Vec<String>, Vec<String>)>>, // (read, write) queued before keys set
}

impl BlossomManager {
    pub fn new() -> Self {
        Self {
            client: RwLock::new(None),
            keys: RwLock::new(None),
            pending_servers: RwLock::new(None),
        }
    }

    /// Set keys for Blossom authentication
    pub fn set_keys(&self, keys: Keys) {
        // Use pending servers if set, otherwise defaults
        let (read_servers, write_servers) = self
            .pending_servers
            .write()
            .take()
            .unwrap_or_else(|| {
                (
                    DEFAULT_READ_SERVERS.iter().map(|s| s.to_string()).collect(),
                    DEFAULT_WRITE_SERVERS.iter().map(|s| s.to_string()).collect(),
                )
            });

        let client = BlossomClient::new_empty(keys.clone())
            .with_read_servers(read_servers)
            .with_write_servers(write_servers);

        *self.client.write() = Some(client);
        *self.keys.write() = Some(keys);
        info!("Blossom client initialized");
    }

    /// Check if client is initialized
    pub fn is_initialized(&self) -> bool {
        self.client.read().is_some()
    }

    /// Upload data to Blossom servers
    /// Returns the SHA256 hash of the uploaded data
    pub async fn upload(&self, data: &[u8]) -> Result<String, BlossomError> {
        let client = self
            .client
            .read()
            .clone()
            .ok_or_else(|| BlossomError::NoServers)?;

        let (hash, was_new) = client.upload_if_missing(data).await?;

        if was_new {
            info!("Uploaded {} bytes, hash: {}...", data.len(), &hash[..12]);
        } else {
            debug!("Blob already exists: {}...", &hash[..12]);
        }

        Ok(hash)
    }

    /// Download data by hash from Blossom servers
    pub async fn download(&self, hash: &str) -> Result<Vec<u8>, BlossomError> {
        let client = self
            .client
            .read()
            .clone()
            .ok_or_else(|| BlossomError::NoServers)?;

        let data = client.download(hash).await?;
        debug!("Downloaded {} bytes for hash {}...", data.len(), &hash[..12]);

        Ok(data)
    }

    /// Check if a blob exists on any server
    pub async fn exists(&self, hash: &str) -> Result<bool, BlossomError> {
        let client = self
            .client
            .read()
            .clone()
            .ok_or_else(|| BlossomError::NoServers)?;

        Ok(client.exists(hash).await)
    }

    /// Get list of configured read servers
    pub fn read_servers(&self) -> Vec<String> {
        self.client
            .read()
            .as_ref()
            .map(|c| c.read_servers().to_vec())
            .unwrap_or_default()
    }

    /// Get list of configured write servers
    pub fn write_servers(&self) -> Vec<String> {
        self.client
            .read()
            .as_ref()
            .map(|c| c.write_servers().to_vec())
            .unwrap_or_default()
    }

    /// Set custom read and write servers
    /// If keys not set yet, queues the servers for when they are
    pub fn set_servers(
        &self,
        read_servers: Vec<String>,
        write_servers: Vec<String>,
    ) -> Result<(), String> {
        let keys = self.keys.read().clone();

        if let Some(keys) = keys {
            // Keys available - update client now
            let client = BlossomClient::new_empty(keys)
                .with_read_servers(read_servers.clone())
                .with_write_servers(write_servers.clone());

            *self.client.write() = Some(client);
            info!(
                "Blossom servers updated: {} read, {} write",
                read_servers.len(),
                write_servers.len()
            );
        } else {
            // Queue for when keys are set
            *self.pending_servers.write() = Some((read_servers.clone(), write_servers.clone()));
            debug!(
                "Blossom servers queued: {} read, {} write",
                read_servers.len(),
                write_servers.len()
            );
        }
        Ok(())
    }
}

impl Default for BlossomManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_manager() {
        let manager = BlossomManager::new();
        assert!(!manager.is_initialized());
    }

    #[test]
    fn test_set_keys() {
        let manager = BlossomManager::new();
        let keys = Keys::generate();
        manager.set_keys(keys);
        assert!(manager.is_initialized());
    }

    #[test]
    fn test_servers_after_init() {
        let manager = BlossomManager::new();
        let keys = Keys::generate();
        manager.set_keys(keys);

        let read = manager.read_servers();
        let write = manager.write_servers();

        assert!(!read.is_empty());
        assert!(!write.is_empty());
        assert!(read.contains(&"https://cdn.iris.to".to_string()));
        assert!(write.contains(&"https://upload.iris.to".to_string()));
    }

    #[test]
    fn test_servers_before_init() {
        let manager = BlossomManager::new();
        assert!(manager.read_servers().is_empty());
        assert!(manager.write_servers().is_empty());
    }
}
