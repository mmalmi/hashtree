use std::fs;
use std::path::{Path, PathBuf};
#[cfg(feature = "secure-nostr-blossom")]
use std::time::Duration;

#[cfg(feature = "secure-nostr-blossom")]
use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::Store;
#[cfg(feature = "secure-nostr-blossom")]
use hashtree_core::{HashTree, HashTreeConfig};
use hashtree_resolver::RootResolver;
#[cfg(feature = "secure-nostr-blossom")]
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    Keys as HashtreeResolverKeys,
};
use serde::{Deserialize, Serialize};

use crate::error::UpdateError;
use crate::manifest::{UpdateAsset, UpdateManifest};
use crate::reference::UpdateRef;
use crate::target::UpdateTarget;
use crate::updater::{DownloadOptions, HashtreeUpdater, UpdateCheck, UpdateCheckOptions};

pub const SECURE_SOURCE_NAME: &str = "hashtree-nostr-blossom";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProductUpdateMode {
    Cli,
    App,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductAssetPolicy {
    cli_asset_prefix: String,
    cli_display_name: String,
    app_display_name: String,
    app_asset_suffixes: Vec<String>,
    download_file_name_fallback: String,
}

impl ProductAssetPolicy {
    #[must_use]
    pub fn new(
        cli_asset_prefix: impl Into<String>,
        cli_display_name: impl Into<String>,
        app_display_name: impl Into<String>,
    ) -> Self {
        let cli_asset_prefix = cli_asset_prefix.into();
        Self {
            download_file_name_fallback: format!("{cli_asset_prefix}-update"),
            cli_asset_prefix,
            cli_display_name: cli_display_name.into(),
            app_display_name: app_display_name.into(),
            app_asset_suffixes: platform_app_asset_suffixes()
                .iter()
                .map(|suffix| (*suffix).to_string())
                .collect(),
        }
    }

    #[must_use]
    pub fn with_app_asset_suffixes<I, S>(mut self, suffixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.app_asset_suffixes = suffixes
            .into_iter()
            .map(|suffix| suffix.as_ref().trim().to_ascii_lowercase())
            .filter(|suffix| !suffix.is_empty())
            .collect();
        self
    }

    #[must_use]
    pub fn with_download_file_name_fallback(mut self, fallback: impl Into<String>) -> Self {
        self.download_file_name_fallback = fallback.into();
        self
    }

    #[must_use]
    pub fn noun(&self, mode: ProductUpdateMode) -> &str {
        match mode {
            ProductUpdateMode::Cli => &self.cli_display_name,
            ProductUpdateMode::App => &self.app_display_name,
        }
    }

    #[must_use]
    pub fn cli_asset_prefix(&self) -> &str {
        &self.cli_asset_prefix
    }

    #[must_use]
    pub fn app_asset_suffixes(&self) -> &[String] {
        &self.app_asset_suffixes
    }

    #[must_use]
    pub fn download_file_name_fallback(&self) -> &str {
        &self.download_file_name_fallback
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProductUpdateResult {
    pub available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub tag: String,
    pub asset: String,
    pub source: String,
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_cid: Option<String>,
}

pub struct ProductUpdateSelection<R, S: Store> {
    pub updater: HashtreeUpdater<R, S>,
    pub check: UpdateCheck,
    pub asset: UpdateAsset,
    pub tag: String,
    pub update_available: bool,
}

pub async fn select_product_update<R, S>(
    updater: HashtreeUpdater<R, S>,
    reference: UpdateRef,
    current_version: &str,
    mode: ProductUpdateMode,
    policy: &ProductAssetPolicy,
) -> Result<ProductUpdateSelection<R, S>, UpdateError>
where
    R: RootResolver,
    S: Store,
{
    let mut check = updater
        .check(UpdateCheckOptions {
            reference,
            current_version: current_version.to_string(),
            target: UpdateTarget::new(current_archive_target()),
            ..UpdateCheckOptions::default()
        })
        .await?;
    let asset = preferred_product_asset(&check.manifest, mode, policy).ok_or_else(|| {
        UpdateError::AssetNotFound(format!(
            "{} for {}",
            policy.noun(mode),
            current_archive_target()
        ))
    })?;
    check.asset = Some(asset.clone());
    let tag = display_manifest_tag(&check.manifest);
    let update_available = check.update_available;
    Ok(ProductUpdateSelection {
        updater,
        check,
        asset,
        tag,
        update_available,
    })
}

pub fn product_result_from_selection<R, S: Store>(
    current_version: &str,
    selection: &ProductUpdateSelection<R, S>,
    source: impl Into<String>,
    verified: bool,
    path: Option<&Path>,
) -> ProductUpdateResult {
    ProductUpdateResult {
        available: selection.update_available,
        current_version: current_version.to_string(),
        latest_version: selection.tag.trim_start_matches('v').to_string(),
        tag: selection.tag.clone(),
        asset: selection.asset.name.clone(),
        source: source.into(),
        verified,
        path: path.map(|value| value.display().to_string()),
        root_cid: Some(selection.check.root_cid.to_string()),
        release_cid: Some(selection.check.release_cid.to_string()),
    }
}

pub async fn download_product_selection<R, S: Store>(
    selection: &ProductUpdateSelection<R, S>,
    download_dir: Option<&Path>,
    policy: &ProductAssetPolicy,
) -> Result<PathBuf, UpdateError>
where
    R: RootResolver,
{
    let destination = selected_download_path(
        download_dir,
        &selection.asset.name,
        policy.download_file_name_fallback(),
    )?;
    let downloaded = selection
        .updater
        .download(&selection.check, DownloadOptions::default(), None)
        .await?;
    write_downloaded_asset(&destination, &downloaded.bytes)?;
    Ok(destination)
}

#[must_use]
pub fn preferred_product_asset(
    manifest: &UpdateManifest,
    mode: ProductUpdateMode,
    policy: &ProductAssetPolicy,
) -> Option<UpdateAsset> {
    match mode {
        ProductUpdateMode::Cli => preferred_cli_asset_for_target(
            manifest,
            policy.cli_asset_prefix(),
            current_archive_target(),
        ),
        ProductUpdateMode::App => {
            preferred_app_asset_for_suffixes(manifest, policy.app_asset_suffixes())
        }
    }
}

#[must_use]
pub fn preferred_cli_asset_for_target(
    manifest: &UpdateManifest,
    cli_asset_prefix: &str,
    target: &str,
) -> Option<UpdateAsset> {
    let tag = display_manifest_tag(manifest);
    let archive_ext = archive_extension_for_target(target);
    let exact = format!("{cli_asset_prefix}-{tag}-{target}{archive_ext}");
    let unversioned = format!("{cli_asset_prefix}-{target}{archive_ext}");
    let update_target = UpdateTarget::new(target);

    manifest
        .assets
        .iter()
        .find(|asset| asset.name == exact)
        .or_else(|| {
            manifest
                .assets
                .iter()
                .find(|asset| asset.name == unversioned)
        })
        .or_else(|| {
            manifest.assets.iter().find(|asset| {
                asset.name.starts_with(&format!("{cli_asset_prefix}-"))
                    && asset.name.ends_with(archive_ext)
                    && (asset.name.contains(target)
                        || asset.matches_target_with_inference(&update_target))
            })
        })
        .cloned()
}

#[must_use]
pub fn preferred_app_asset_for_suffixes<S: AsRef<str>>(
    manifest: &UpdateManifest,
    suffixes: &[S],
) -> Option<UpdateAsset> {
    if suffixes.is_empty() {
        return None;
    }
    manifest
        .assets
        .iter()
        .find(|asset| {
            let lower = asset.name.to_ascii_lowercase();
            suffixes.iter().any(|suffix| {
                let suffix = suffix.as_ref().trim().to_ascii_lowercase();
                !suffix.is_empty() && lower.ends_with(&suffix)
            })
        })
        .cloned()
}

#[must_use]
pub fn display_manifest_tag(manifest: &UpdateManifest) -> String {
    manifest
        .tag
        .clone()
        .filter(|tag| !tag.trim().is_empty())
        .unwrap_or_else(|| format!("v{}", manifest.effective_version()))
}

#[must_use]
pub fn archive_extension_for_target(target: &str) -> &'static str {
    if target.contains("windows") {
        ".zip"
    } else {
        ".tar.gz"
    }
}

#[must_use]
pub fn platform_app_asset_suffixes() -> &'static [&'static str] {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &[
            "-macos-arm64.app.tar.gz",
            "-macos-arm64.dmg",
            "-macos-arm64.zip",
        ]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &["-macos-x64.app.tar.gz", "-macos-x64.dmg", "-macos-x64.zip"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["-linux-x64.appimage", "-linux-x64.deb"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["-linux-arm64.appimage", "-linux-arm64.deb"]
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        &["-windows-x64-setup.exe"]
    }
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    {
        &["-android-arm64.apk"]
    }
    #[cfg(not(any(
        all(
            target_os = "macos",
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "android", target_arch = "aarch64"),
    )))]
    {
        &[]
    }
}

#[must_use]
pub fn current_archive_target() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "linux", target_arch = "arm"))]
    {
        "arm-unknown-linux-musleabihf"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        "aarch64-pc-windows-msvc"
    }
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    {
        "aarch64-linux-android"
    }
    #[cfg(not(any(
        all(
            target_os = "macos",
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "arm")
        ),
        all(
            target_os = "windows",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "android", target_arch = "aarch64"),
    )))]
    {
        "unsupported"
    }
}

pub fn selected_download_path(
    download_dir: Option<&Path>,
    asset_name: &str,
    fallback_name: &str,
) -> Result<PathBuf, UpdateError> {
    let file_name = safe_download_file_name(asset_name, fallback_name);
    let parent = download_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&parent)?;
    Ok(parent.join(file_name))
}

pub fn write_downloaded_asset(destination: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(destination, bytes)?;
    Ok(())
}

#[must_use]
pub fn safe_download_file_name(name: &str, fallback_name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        fallback_name.to_string()
    } else {
        out
    }
}

#[must_use]
pub fn env_csv(name: &str) -> Option<Vec<String>> {
    std::env::var(name)
        .ok()
        .map(|value| split_csv(&value))
        .filter(|values| !values.is_empty())
}

#[must_use]
pub fn split_csv(value: &str) -> Vec<String> {
    dedupe_nonempty(
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

#[must_use]
pub fn dedupe_nonempty(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

pub fn update_ref_from_override(
    override_ref: Option<&str>,
    env_name: Option<&str>,
    default_ref: &str,
) -> Result<UpdateRef, UpdateError> {
    let env_ref = env_name
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty());
    let raw = override_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(env_ref.as_deref())
        .unwrap_or(default_ref);
    UpdateRef::parse(raw).map_err(|error| {
        UpdateError::InvalidReference(format!("invalid update hashtree ref {raw}: {error}"))
    })
}

#[cfg(feature = "secure-nostr-blossom")]
pub type SecureNostrBlossomUpdater = HashtreeUpdater<NostrRootResolver, BlossomStore>;

#[cfg(feature = "secure-nostr-blossom")]
pub type SecureNostrBlossomSelection = ProductUpdateSelection<NostrRootResolver, BlossomStore>;

#[cfg(feature = "secure-nostr-blossom")]
#[derive(Clone, Debug)]
pub struct SecureNostrBlossomConfig {
    pub relays: Vec<String>,
    pub blossom_read_servers: Vec<String>,
    pub manifest_timeout: Duration,
    pub download_timeout: Duration,
}

#[cfg(feature = "secure-nostr-blossom")]
impl Default for SecureNostrBlossomConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            blossom_read_servers: Vec::new(),
            manifest_timeout: Duration::from_secs(8),
            download_timeout: Duration::from_secs(180),
        }
    }
}

#[cfg(feature = "secure-nostr-blossom")]
pub async fn build_secure_nostr_blossom_updater(
    config: SecureNostrBlossomConfig,
) -> Result<SecureNostrBlossomUpdater, UpdateError> {
    let resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: config.relays,
        resolve_timeout: config.manifest_timeout,
        secret_key: None,
    })
    .await?;
    let blossom = BlossomClient::new_empty(HashtreeResolverKeys::generate())
        .with_read_servers(config.blossom_read_servers)
        .with_timeout(config.download_timeout);
    let store = std::sync::Arc::new(BlossomStore::new(blossom));
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    Ok(HashtreeUpdater::new(resolver, tree))
}
