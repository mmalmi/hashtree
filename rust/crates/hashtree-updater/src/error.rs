use hashtree_core::HashTreeError;
use hashtree_resolver::ResolverError;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("invalid update reference: {0}")]
    InvalidReference(String),
    #[error("failed to resolve update root: {0}")]
    Resolve(#[from] ResolverError),
    #[error("update announcement failed: {0}")]
    Announcement(String),
    #[error("hashtree read failed: {0}")]
    Tree(#[from] HashTreeError),
    #[error("release root was not found for {0}")]
    ReleaseNotFound(String),
    #[error("manifest was not found at {0}")]
    ManifestNotFound(String),
    #[error("manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error("failed to decode manifest: {0}")]
    ManifestJson(#[from] serde_json::Error),
    #[error("invalid version: {0}")]
    Version(#[from] semver::Error),
    #[error("no update asset matched target {0}")]
    AssetNotFound(String),
    #[error("check result does not contain a selected asset")]
    NoSelectedAsset,
    #[error("asset was not found at {0}")]
    AssetPathNotFound(String),
    #[error("asset kind {kind} is not supported on this platform")]
    UnsupportedKind { kind: String },
    #[error("install failed: {0}")]
    Install(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
