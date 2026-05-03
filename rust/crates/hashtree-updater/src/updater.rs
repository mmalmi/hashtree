use hashtree_core::{Cid, HashTree, Store};
use hashtree_resolver::RootResolver;

use crate::error::UpdateError;
use crate::manifest::{UpdateAsset, UpdateManifest};
use crate::progress::{DownloadCallback, DownloadEvent};
use crate::reference::UpdateRef;
use crate::target::UpdateTarget;

const DEFAULT_MANIFEST_PATH: &str = "release.json";
const DEFAULT_MAX_MANIFEST_SIZE: u64 = 1024 * 1024;
const DEFAULT_PROGRESS_CHUNK: u64 = 256 * 1024;

#[derive(Debug, Clone)]
pub struct UpdateCheckOptions {
    pub reference: UpdateRef,
    pub current_version: String,
    pub target: UpdateTarget,
    pub manifest_path: String,
    pub max_manifest_size: u64,
}

impl Default for UpdateCheckOptions {
    fn default() -> Self {
        Self {
            reference: UpdateRef::default(),
            current_version: "0.0.0".to_string(),
            target: UpdateTarget::current(),
            manifest_path: DEFAULT_MANIFEST_PATH.to_string(),
            max_manifest_size: DEFAULT_MAX_MANIFEST_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateCheck {
    pub root_cid: Cid,
    pub release_cid: Cid,
    pub manifest_path: String,
    pub manifest: UpdateManifest,
    pub asset: Option<UpdateAsset>,
    pub update_available: bool,
}

#[derive(Debug, Clone)]
pub struct DownloadedAsset {
    pub asset: UpdateAsset,
    pub cid: Cid,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    /// Refuse to download if the asset is larger than this many bytes.
    pub max_size: Option<u64>,
    /// Approximate bytes between `Progress` events. Defaults to 256 KiB.
    pub progress_chunk: Option<u64>,
}

pub struct HashtreeUpdater<R, S: Store> {
    resolver: R,
    tree: HashTree<S>,
}

impl<R, S> HashtreeUpdater<R, S>
where
    R: RootResolver,
    S: Store,
{
    pub fn new(resolver: R, tree: HashTree<S>) -> Self {
        Self { resolver, tree }
    }

    pub fn tree(&self) -> &HashTree<S> {
        &self.tree
    }

    pub async fn check(&self, options: UpdateCheckOptions) -> Result<UpdateCheck, UpdateError> {
        if options.reference.npub.is_empty() || options.reference.tree_name.is_empty() {
            return Err(UpdateError::InvalidReference(
                "reference must contain npub and tree name".to_string(),
            ));
        }

        let resolver_key = options.reference.resolver_key();
        let root_cid = self
            .resolver
            .resolve(&resolver_key)
            .await?
            .ok_or_else(|| UpdateError::ReleaseNotFound(resolver_key.clone()))?;
        let release_cid = match options.reference.path.as_deref() {
            Some(path) => self
                .tree
                .resolve_path(&root_cid, path)
                .await?
                .ok_or_else(|| UpdateError::ReleaseNotFound(path.to_string()))?,
            None => root_cid.clone(),
        };

        let manifest_cid = self
            .tree
            .resolve_path(&release_cid, &options.manifest_path)
            .await?
            .ok_or_else(|| UpdateError::ManifestNotFound(options.manifest_path.clone()))?;
        let manifest_bytes = self
            .tree
            .get(&manifest_cid, Some(options.max_manifest_size))
            .await?
            .ok_or_else(|| UpdateError::ManifestNotFound(options.manifest_path.clone()))?;
        let manifest: UpdateManifest = serde_json::from_slice(&manifest_bytes)?;
        manifest.validate()?;

        let asset = manifest
            .select_asset(&options.target)
            .ok_or_else(|| UpdateError::AssetNotFound(options.target.as_str().to_string()))?
            .clone();
        let update_available = manifest.is_newer_than(&options.current_version)?;

        Ok(UpdateCheck {
            root_cid,
            release_cid,
            manifest_path: options.manifest_path,
            manifest,
            asset: Some(asset),
            update_available,
        })
    }

    /// Download the selected asset, emitting progress to the optional callback.
    ///
    /// The hashtree underneath verifies every chunk against its CID, so the
    /// returned bytes are already authenticated.
    pub async fn download(
        &self,
        check: &UpdateCheck,
        options: DownloadOptions,
        on_event: Option<DownloadCallback>,
    ) -> Result<DownloadedAsset, UpdateError> {
        let asset = check.asset.clone().ok_or(UpdateError::NoSelectedAsset)?;
        asset.validate()?;
        let cid = self
            .tree
            .resolve_path(&check.release_cid, &asset.path)
            .await?
            .ok_or_else(|| UpdateError::AssetPathNotFound(asset.path.clone()))?;

        let total = self.tree.get_size_cid(&cid).await.ok();
        if let (Some(limit), Some(size)) = (options.max_size, total) {
            if size > limit {
                return Err(UpdateError::Install(format!(
                    "asset size {size} exceeds max_size {limit}",
                )));
            }
        }

        emit(&on_event, DownloadEvent::Started { content_length: total });

        let bytes = match total {
            Some(size) if size > 0 => {
                self.read_with_progress(&cid, size, &options, &on_event).await?
            }
            _ => {
                let bytes = self
                    .tree
                    .get(&cid, options.max_size)
                    .await?
                    .ok_or_else(|| UpdateError::AssetPathNotFound(asset.path.clone()))?;
                emit(
                    &on_event,
                    DownloadEvent::Progress {
                        chunk_len: bytes.len() as u64,
                        downloaded: bytes.len() as u64,
                    },
                );
                bytes
            }
        };

        emit(
            &on_event,
            DownloadEvent::Finished {
                total: bytes.len() as u64,
            },
        );

        Ok(DownloadedAsset { asset, cid, bytes })
    }

    /// Backwards-compatible wrapper around `download` with no progress callback.
    pub async fn download_asset(
        &self,
        check: &UpdateCheck,
        max_size: Option<u64>,
    ) -> Result<DownloadedAsset, UpdateError> {
        self.download(
            check,
            DownloadOptions { max_size, ..Default::default() },
            None,
        )
        .await
    }

    async fn read_with_progress(
        &self,
        cid: &Cid,
        total: u64,
        options: &DownloadOptions,
        on_event: &Option<DownloadCallback>,
    ) -> Result<Vec<u8>, UpdateError> {
        let chunk = options.progress_chunk.unwrap_or(DEFAULT_PROGRESS_CHUNK).max(1);
        let mut buf = Vec::with_capacity(total as usize);
        let mut offset = 0u64;
        while offset < total {
            let end = (offset + chunk).min(total);
            let part = self
                .tree
                .read_file_range_cid(cid, offset, Some(end))
                .await?
                .unwrap_or_default();
            let part_len = part.len() as u64;
            if part_len == 0 {
                break;
            }
            buf.extend_from_slice(&part);
            offset += part_len;
            emit(
                on_event,
                DownloadEvent::Progress {
                    chunk_len: part_len,
                    downloaded: offset,
                },
            );
        }
        Ok(buf)
    }
}

fn emit(cb: &Option<DownloadCallback>, event: DownloadEvent) {
    if let Some(cb) = cb {
        cb(event);
    }
}
