use crate::{Error, Result, Torrent};
use async_trait::async_trait;
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, StoreError, BLOB_MAX_BYTES,
};
use std::collections::HashMap;

/// A source hint for exact stored bytes. A mapping never grants decryption access.
#[derive(Clone, Debug)]
pub struct BlobMapping {
    pub hash: Hash,
    pub file: usize,
    pub offset: u64,
    pub length: usize,
}

pub struct TorrentRoute {
    torrent: Torrent,
    mappings: HashMap<Hash, BlobMapping>,
}
impl TorrentRoute {
    pub fn new(torrent: Torrent, mappings: Vec<BlobMapping>) -> Result<Self> {
        if mappings.len() > 1_000_000 {
            return Err(Error::Invalid("too many blob mappings".into()));
        }
        let mut index = HashMap::new();
        for mapping in mappings {
            let size = torrent.metadata().file(mapping.file)?.size;
            if mapping.length > BLOB_MAX_BYTES
                || mapping.offset > size
                || mapping.length as u64 > size - mapping.offset
            {
                return Err(Error::Invalid("invalid blob mapping range".into()));
            }
            index.entry(mapping.hash).or_insert(mapping);
        }
        Ok(Self {
            torrent,
            mappings: index,
        })
    }
}
#[async_trait]
impl BlobRoute for TorrentRoute {
    async fn route(&self, request: BlobRequest) -> std::result::Result<BlobReply, StoreError> {
        let Some(mapping) = self.mappings.get(&request.hash) else {
            return Ok(BlobReply::NoResult);
        };
        let bytes = self
            .torrent
            .read_range(mapping.file, mapping.offset, mapping.length)
            .await
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if sha256(&bytes) != request.hash {
            return Err(StoreError::Other(
                "torrent range failed hashtree SHA-256 verification".into(),
            ));
        }
        Ok(BlobReply::Data(bytes))
    }
    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> std::result::Result<BlobReply, StoreError> {
        let remaining = context
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        tokio::time::timeout(remaining, self.route(request))
            .await
            .map_err(|_| {
                StoreError::Other("torrent route timed out; availability is unknown".into())
            })?
    }
}
