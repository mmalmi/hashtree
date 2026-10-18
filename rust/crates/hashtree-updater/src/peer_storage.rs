//! Reuse application storage and peer routes for public software updates.
use std::sync::Arc;

use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::{BlobRoute, Store, StoreBlobRoute};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig, RoutedStore};
use hashtree_resolver::Keys;

use crate::{SecurePubsubBlossomConfig, UpdateError};

pub type UpdateStore = RoutedStore<dyn Store>;

/// Verified reads populate the application's primary store, making downloaded
/// blocks available through its existing blob service. Writes never go to a
/// discovered store or peer. Retention and serving permissions stay with the
/// application; only raw hash-addressed blocks are cached, never signing keys.
///
/// A quiet or corrupt peer cannot veto another available route. Empty Blossom
/// configuration supports fully peer-to-peer retrieval without an HTTP source.
pub fn build_update_store(
    primary: Arc<dyn Store>,
    peers: Option<Arc<dyn BlobRoute>>,
    config: SecurePubsubBlossomConfig,
) -> Result<Arc<UpdateStore>, UpdateError> {
    let mut routes = vec![BlobRouteEntry::new(
        "update-cache",
        Arc::new(StoreBlobRoute::new(primary.clone())),
    )];
    if let Some(peers) = peers {
        routes.push(BlobRouteEntry::new("update-peers", peers));
    }
    if !config.blossom_read_servers.is_empty() {
        let blossom = BlossomClient::new_empty(Keys::generate())
            .with_read_servers(config.blossom_read_servers)
            .with_timeout(config.download_timeout);
        routes.push(BlobRouteEntry::new(
            "update-blossom",
            Arc::new(StoreBlobRoute::new(Arc::new(BlossomStore::new(blossom)))),
        ));
    }
    let router = BlobRouter::new(
        routes,
        Some(primary.clone()),
        BlobRouterConfig {
            request_timeout: config.download_timeout,
            ..Default::default()
        },
    )
    .map_err(|error| UpdateError::Tree(hashtree_core::HashTreeError::Store(error.to_string())))?;
    Ok(Arc::new(RoutedStore::new(primary, Arc::new(router))))
}
