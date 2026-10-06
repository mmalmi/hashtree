//! Verified public Hashtree reads, sharing an existing daemon when available.
//! No daemon database is opened and no background service is started.

mod config;
mod store;
pub use config::ClientConfig;
pub use store::BlobStore;

use anyhow::{ensure, Context, Result};
use hashtree_core::{Cid, HashTree, HashTreeConfig};
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    RootResolver,
};
use nostr::{nips::nip19::ToBech32, PublicKey};
use percent_encoding::percent_decode_str;
use std::{path::Path, sync::Arc};
use tokio::sync::OnceCell;

/// A public mutable tree reference. The first path segment is the tree name;
/// percent-encoded slashes belong to that name. Later segments select a directory.
#[derive(Clone, Debug)]
pub struct Reference {
    pub key: String,
    pub path: String,
}

impl Reference {
    pub fn parse(value: &str) -> Result<Self> {
        let url = reqwest::Url::parse(value)?;
        ensure!(
            url.scheme() == "htree"
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "expected a public htree:// publisher/tree URL"
        );
        let author = PublicKey::parse(url.host_str().context("missing publisher")?)?.to_bech32()?;
        let mut parts = url.path().trim_start_matches('/').splitn(2, '/');
        let tree = percent_decode_str(parts.next().unwrap_or_default()).decode_utf8()?;
        ensure!(!tree.is_empty(), "missing Hashtree tree name");
        let path = percent_decode_str(parts.next().unwrap_or_default())
            .decode_utf8()?
            .into_owned();
        ensure!(
            !path
                .split('/')
                .any(|s| s == "." || s == ".." || s.contains('\\') || s.contains('\0')),
            "invalid directory path"
        );
        Ok(Self {
            key: format!("{author}/{tree}"),
            path,
        })
    }
}

pub struct Client {
    config: ClientConfig,
    store: Arc<BlobStore>,
    daemon: OnceCell<Option<String>>,
}

impl Client {
    pub fn new(config: ClientConfig, cache: &Path) -> Result<Self> {
        let store = Arc::new(BlobStore::new(cache, config.clone())?);
        Ok(Self {
            config,
            store,
            daemon: OnceCell::new(),
        })
    }

    pub fn store(&self) -> Arc<BlobStore> {
        self.store.clone()
    }

    pub async fn daemon_url(&self) -> Option<&str> {
        self.daemon
            .get_or_init(|| async {
                let url = self.config.daemon_url.as_ref()?;
                let response = self
                    .store
                    .http
                    .get(format!("{url}/health"))
                    .timeout(std::time::Duration::from_millis(500))
                    .send()
                    .await
                    .ok()?;
                response.status().is_success().then(|| url.clone())
            })
            .await
            .as_deref()
    }

    async fn observe(&self, key: &str, relays: Vec<String>) -> Result<Cid> {
        let resolver = NostrRootResolver::new(NostrResolverConfig {
            relays,
            resolve_timeout: self.config.resolve_window,
            secret_key: None,
        })
        .await?;
        let result = resolver.resolve_open(key, self.config.resolve_window).await;
        resolver.stop().await?;
        Ok(result?)
    }

    /// Observe signed roots for the configured window, including after EOSE.
    /// A quiet/failed local subscription falls back to configured relays unless
    /// local-daemon-only was explicitly requested. Absence is reported as timeout.
    pub async fn resolve(&self, reference: &Reference) -> Result<Cid> {
        let local_result = if let Some(daemon) = self.daemon_url().await {
            let mut url = reqwest::Url::parse(daemon)?;
            url.set_scheme("ws")
                .map_err(|_| anyhow::anyhow!("invalid local daemon URL"))?;
            url.set_path("/ws");
            Some(self.observe(&reference.key, vec![url.to_string()]).await)
        } else {
            None
        };
        let root = match local_result {
            Some(Ok(root)) => root,
            Some(Err(error)) if self.config.local_only => return Err(error),
            None if self.config.local_only => {
                anyhow::bail!("HTREE_LOCAL_DAEMON_ONLY requires a running local htree daemon")
            }
            _ => {
                self.observe(&reference.key, self.config.relays.clone())
                    .await?
            }
        };
        if reference.path.is_empty() {
            return Ok(root);
        }
        self.tree()
            .resolve(&root, &reference.path)
            .await?
            .context("Hashtree directory is missing")
    }

    fn tree(&self) -> HashTree<BlobStore> {
        HashTree::new(HashTreeConfig::new(self.store.clone()))
    }

    /// Read a logical file from an immutable root, verifying every raw block.
    pub async fn read_file(&self, root: &Cid, path: &str, limit: usize) -> Result<Vec<u8>> {
        let cid = self
            .tree()
            .resolve(root, path)
            .await?
            .context("Hashtree file is missing")?;
        self.tree()
            .get(&cid, Some(limit as u64))
            .await?
            .context("Hashtree file is unavailable")
    }
}
