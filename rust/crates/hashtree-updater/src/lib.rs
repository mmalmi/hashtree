//! Hashtree-based update discovery and artifact installation helpers.
//!
//! This crate treats a signed hashtree mutable root as the update authority.
//! Apps bake an `npub` + release tree + channel/path, resolve it to an
//! immutable release directory, read `manifest.json`, select the asset for the
//! current platform, then download or install that asset locally.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hashtree_core::{from_hex, sha256, to_hex, Cid, HashTree, HashTreeError, Store};
use hashtree_resolver::{ResolverError, RootResolver};
use semver::Version;
use serde::{Deserialize, Serialize};

const DEFAULT_MANIFEST_PATH: &str = "manifest.json";
const DEFAULT_MAX_MANIFEST_SIZE: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("invalid update reference: {0}")]
    InvalidReference(String),
    #[error("failed to resolve update root: {0}")]
    Resolve(#[from] ResolverError),
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
    #[error("asset size mismatch for {path}: expected {expected} bytes, got {actual} bytes")]
    AssetSizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("asset sha256 mismatch for {path}: expected {expected}, got {actual}")]
    AssetHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateRef {
    pub npub: String,
    pub tree_name: String,
    pub path: Option<String>,
}

impl UpdateRef {
    pub fn parse(input: &str) -> Result<Self, UpdateError> {
        let input = input.strip_prefix("htree://").unwrap_or(input);
        let input = input.split('#').next().unwrap_or(input);
        let input = input.split('?').next().unwrap_or(input).trim_matches('/');

        if !input.starts_with("npub1") {
            return Err(UpdateError::InvalidReference(
                "expected npub/path or htree://npub/path".to_string(),
            ));
        }

        let mut parts = input.split('/');
        let npub = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| UpdateError::InvalidReference("missing npub".to_string()))?;
        let tree_name = parts
            .next()
            .map(decode_reference_segment)
            .filter(|part| !part.is_empty())
            .ok_or_else(|| UpdateError::InvalidReference("missing tree name".to_string()))?;
        let path_parts = parts.map(decode_reference_segment).collect::<Vec<_>>();

        Ok(Self {
            npub: npub.to_string(),
            tree_name,
            path: (!path_parts.is_empty()).then(|| path_parts.join("/")),
        })
    }

    pub fn resolver_key(&self) -> String {
        format!("{}/{}", self.npub, self.tree_name)
    }
}

fn decode_reference_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = bytes[index + 1] as char;
            let lo = bytes[index + 2] as char;
            if let (Some(hi), Some(lo)) = (hi.to_digit(16), lo.to_digit(16)) {
                decoded.push(((hi << 4) | lo) as u8);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|_| segment.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateTarget {
    target: String,
    aliases: BTreeSet<String>,
}

impl UpdateTarget {
    pub fn new(target: impl Into<String>) -> Self {
        let target = normalize_target(&target.into());
        let aliases = target_aliases(&target);
        Self { target, aliases }
    }

    pub fn current() -> Self {
        Self::new(current_target())
    }

    pub fn as_str(&self) -> &str {
        &self.target
    }

    pub fn matches(&self, candidate: &str) -> bool {
        let candidate_aliases = target_aliases(&normalize_target(candidate));
        self.aliases
            .iter()
            .any(|alias| candidate_aliases.contains(alias))
    }
}

impl Default for UpdateTarget {
    fn default() -> Self {
        Self::current()
    }
}

fn normalize_target(target: &str) -> String {
    target.trim().to_ascii_lowercase()
}

fn target_aliases(target: &str) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    aliases.insert(target.to_string());

    match target {
        "aarch64-apple-darwin"
        | "darwin-aarch64"
        | "darwin-arm64"
        | "macos-aarch64"
        | "macos-arm64" => {
            aliases.extend(
                [
                    "aarch64-apple-darwin",
                    "darwin-aarch64",
                    "darwin-arm64",
                    "macos-aarch64",
                    "macos-arm64",
                ]
                .into_iter()
                .map(String::from),
            );
        }
        "x86_64-apple-darwin" | "darwin-x86_64" | "macos-x86_64" | "macos-x64" => {
            aliases.extend(
                [
                    "x86_64-apple-darwin",
                    "darwin-x86_64",
                    "macos-x86_64",
                    "macos-x64",
                ]
                .into_iter()
                .map(String::from),
            );
        }
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" | "linux-x86_64" | "linux-x64" => {
            aliases.extend(
                [
                    "x86_64-unknown-linux-gnu",
                    "x86_64-unknown-linux-musl",
                    "linux-x86_64",
                    "linux-x64",
                ]
                .into_iter()
                .map(String::from),
            );
        }
        "aarch64-unknown-linux-gnu"
        | "aarch64-unknown-linux-musl"
        | "linux-aarch64"
        | "linux-arm64" => {
            aliases.extend(
                [
                    "aarch64-unknown-linux-gnu",
                    "aarch64-unknown-linux-musl",
                    "linux-aarch64",
                    "linux-arm64",
                ]
                .into_iter()
                .map(String::from),
            );
        }
        "x86_64-pc-windows-msvc" | "windows-x86_64" | "windows-x64" => {
            aliases.extend(
                ["x86_64-pc-windows-msvc", "windows-x86_64", "windows-x64"]
                    .into_iter()
                    .map(String::from),
            );
        }
        "aarch64-pc-windows-msvc" | "windows-aarch64" | "windows-arm64" => {
            aliases.extend(
                [
                    "aarch64-pc-windows-msvc",
                    "windows-aarch64",
                    "windows-arm64",
                ]
                .into_iter()
                .map(String::from),
            );
        }
        _ => {}
    }

    aliases
}

fn current_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateManifest {
    pub schema: Option<String>,
    pub app: String,
    pub version: String,
    pub channel: Option<String>,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    pub min_version: Option<String>,
    pub assets: Vec<UpdateAsset>,
}

impl UpdateManifest {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.app.trim().is_empty() {
            return Err(UpdateError::InvalidManifest("app is required".to_string()));
        }
        if self.version.trim().is_empty() {
            return Err(UpdateError::InvalidManifest(
                "version is required".to_string(),
            ));
        }
        parse_version(&self.version)?;
        if let Some(min_version) = self.min_version.as_deref() {
            parse_version(min_version)?;
        }
        if self.assets.is_empty() {
            return Err(UpdateError::InvalidManifest(
                "at least one asset is required".to_string(),
            ));
        }
        for asset in &self.assets {
            asset.validate()?;
        }
        Ok(())
    }

    pub fn select_asset(&self, target: &UpdateTarget) -> Option<&UpdateAsset> {
        self.assets
            .iter()
            .find(|asset| asset.matches_target(target))
            .or_else(|| {
                (self.assets.len() == 1 && self.assets[0].target_values().is_empty())
                    .then_some(&self.assets[0])
            })
    }

    pub fn is_newer_than(&self, current_version: &str) -> Result<bool, UpdateError> {
        Ok(parse_version(&self.version)? > parse_version(current_version)?)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateAsset {
    pub name: String,
    pub path: String,
    pub target: Option<String>,
    pub targets: Vec<String>,
    pub kind: Option<String>,
    pub executable: Option<String>,
    pub size: Option<u64>,
    #[serde(alias = "hash")]
    pub sha256: Option<String>,
}

impl UpdateAsset {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.name.trim().is_empty() {
            return Err(UpdateError::InvalidManifest(
                "asset name is required".to_string(),
            ));
        }
        if !is_safe_relative_path(&self.path) {
            return Err(UpdateError::InvalidManifest(format!(
                "asset path is not a safe relative path: {}",
                self.path
            )));
        }
        if let Some(hash) = self.sha256.as_deref() {
            from_hex(hash).map_err(|_| {
                UpdateError::InvalidManifest(format!(
                    "asset sha256 must be 64 hex chars: {}",
                    self.name
                ))
            })?;
        }
        Ok(())
    }

    pub fn target_values(&self) -> Vec<&str> {
        self.target
            .as_deref()
            .into_iter()
            .chain(self.targets.iter().map(String::as_str))
            .filter(|value| !value.trim().is_empty())
            .collect()
    }

    pub fn matches_target(&self, target: &UpdateTarget) -> bool {
        self.target_values()
            .into_iter()
            .any(|candidate| target.matches(candidate))
    }
}

fn is_safe_relative_path(path: &str) -> bool {
    let trimmed = path.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with('/')
        && trimmed
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn parse_version(version: &str) -> Result<Version, semver::Error> {
    Version::parse(version.trim().strip_prefix('v').unwrap_or(version.trim()))
}

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

    pub async fn download_asset(
        &self,
        check: &UpdateCheck,
        max_size: Option<u64>,
    ) -> Result<DownloadedAsset, UpdateError> {
        let asset = check.asset.clone().ok_or(UpdateError::NoSelectedAsset)?;
        asset.validate()?;
        let cid = self
            .tree
            .resolve_path(&check.release_cid, &asset.path)
            .await?
            .ok_or_else(|| UpdateError::AssetPathNotFound(asset.path.clone()))?;
        let bytes = self
            .tree
            .get(&cid, max_size)
            .await?
            .ok_or_else(|| UpdateError::AssetPathNotFound(asset.path.clone()))?;

        if let Some(expected) = asset.size {
            let actual = bytes.len() as u64;
            if actual != expected {
                return Err(UpdateError::AssetSizeMismatch {
                    path: asset.path,
                    expected,
                    actual,
                });
            }
        }

        if let Some(expected) = asset.sha256.as_deref() {
            let actual = to_hex(&sha256(&bytes));
            if actual != expected {
                return Err(UpdateError::AssetHashMismatch {
                    path: asset.path,
                    expected: expected.to_string(),
                    actual,
                });
            }
        }

        Ok(DownloadedAsset { asset, cid, bytes })
    }
}

pub fn install_file(
    path: impl AsRef<Path>,
    bytes: &[u8],
    executable: bool,
) -> Result<(), UpdateError> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp_path = temporary_install_path(path);
    std::fs::write(&temp_path, bytes)?;
    if executable {
        make_executable(&temp_path)?;
    }
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

fn temporary_install_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("update");
    let temp_name = format!(".{file_name}.{}.tmp", std::process::id());
    path.with_file_name(temp_name)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}
