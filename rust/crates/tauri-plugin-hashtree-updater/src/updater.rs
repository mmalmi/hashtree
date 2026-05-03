use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::{HashTree, HashTreeConfig};
use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
use hashtree_updater::{
    install, AssetKind, DownloadCallback, DownloadOptions, HashtreeUpdater, InstallTarget,
    UpdateAsset, UpdateCheckOptions, UpdateRef, UpdateTarget,
};
use nostr::Keys;
use tauri::{AppHandle, Manager, Runtime};

use crate::config::Config;
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct CheckedUpdate {
    pub current_version: String,
    pub version: String,
    pub asset_name: String,
    pub asset_kind: String,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    pub update_available: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InstallOverrides {
    /// Final destination path. When `None`, falls back to the plugin config's
    /// `destination`.
    pub destination: Option<PathBuf>,
    /// Override the asset kind reported in the manifest.
    pub kind: Option<String>,
    /// Set the unix executable bit after install (binary kind only).
    pub executable: bool,
}

/// Per-app updater handle. Built fresh on each request because Nostr/Blossom
/// connections are cheap and short-lived.
pub struct UpdaterContext {
    pub config: Config,
    pub current_version: String,
}

impl UpdaterContext {
    pub fn new(config: Config, current_version: String) -> Self {
        Self {
            config,
            current_version,
        }
    }

    fn reference(&self) -> Result<UpdateRef> {
        let raw = self
            .config
            .reference
            .as_deref()
            .ok_or_else(|| Error::Config("reference is required".to_string()))?;
        Ok(UpdateRef::parse(raw)?)
    }

    async fn build_updater(&self) -> Result<HashtreeUpdater<NostrRootResolver, BlossomStore>> {
        let keys = Keys::generate();
        let mut blossom = BlossomClient::new(keys.clone());
        if !self.config.blossom_servers.is_empty() {
            blossom = blossom.with_servers(self.config.blossom_servers.clone());
        }
        let store = Arc::new(BlossomStore::new(blossom));
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let resolver_config = NostrResolverConfig {
            relays: self.config.relays.clone(),
            resolve_timeout: Duration::from_secs(10),
            secret_key: Some(keys),
        };
        let resolver = NostrRootResolver::new(resolver_config).await?;
        Ok(HashtreeUpdater::new(resolver, tree))
    }

    pub async fn check(&self) -> Result<Option<CheckedUpdate>> {
        let updater = self.build_updater().await?;
        let options = UpdateCheckOptions {
            reference: self.reference()?,
            current_version: self.current_version.clone(),
            target: UpdateTarget::current(),
            manifest_path: self
                .config
                .manifest_path
                .clone()
                .unwrap_or_else(|| "manifest.json".to_string()),
            ..UpdateCheckOptions::default()
        };
        let check = updater.check(options).await?;
        let Some(asset) = check.asset.as_ref() else {
            return Ok(None);
        };
        Ok(Some(CheckedUpdate {
            current_version: self.current_version.clone(),
            version: check.manifest.effective_version(),
            asset_name: asset.name.clone(),
            asset_kind: asset.asset_kind().as_str().to_string(),
            published_at: check.manifest.published_at_string(),
            notes: check.manifest.notes,
            update_available: check.update_available,
        }))
    }

    pub async fn download_and_install(
        &self,
        overrides: InstallOverrides,
        on_event: Option<DownloadCallback>,
    ) -> Result<CheckedUpdate> {
        let updater = self.build_updater().await?;
        let options = UpdateCheckOptions {
            reference: self.reference()?,
            current_version: self.current_version.clone(),
            target: UpdateTarget::current(),
            manifest_path: self
                .config
                .manifest_path
                .clone()
                .unwrap_or_else(|| "manifest.json".to_string()),
            ..UpdateCheckOptions::default()
        };
        let check = updater.check(options).await?;
        let mut asset: UpdateAsset = check
            .asset
            .clone()
            .ok_or_else(|| Error::Updater(hashtree_updater::UpdateError::NoSelectedAsset))?;
        if let Some(kind) = overrides.kind.as_deref() {
            if AssetKind::parse(kind).is_none() {
                return Err(Error::Config(format!("unknown asset kind: {kind}")));
            }
            asset.kind = Some(kind.to_string());
        }

        let downloaded = updater
            .download(&check, DownloadOptions::default(), on_event)
            .await?;

        let destination = overrides
            .destination
            .or_else(|| self.config.destination.clone())
            .or_else(|| default_destination_for(asset.asset_kind()))
            .ok_or_else(|| Error::MissingDestination(asset.asset_kind().as_str().to_string()))?;
        let target = InstallTarget::new(destination).executable(overrides.executable);
        install(&asset, &downloaded.bytes, &target)?;

        Ok(CheckedUpdate {
            current_version: self.current_version.clone(),
            version: check.manifest.effective_version(),
            asset_name: asset.name.clone(),
            asset_kind: asset.asset_kind().as_str().to_string(),
            published_at: check.manifest.published_at_string(),
            notes: check.manifest.notes,
            update_available: check.update_available,
        })
    }
}

/// Helper that pulls plugin state out of `tauri::App` and constructs a
/// per-call `UpdaterContext`. Reads `app.package_info().version` for the
/// current version unless overridden.
pub fn context_from_app<R: Runtime>(app: &AppHandle<R>) -> UpdaterContext {
    let state = app.state::<crate::PluginState>();
    let pkg = app.package_info();
    UpdaterContext::new(state.config.clone(), pkg.version.to_string())
}

/// Best-effort default install destination based on the running binary.
/// Returns `None` for kinds the plugin can't install in place (deb, rpm,
/// nsis, msi, archive).
fn default_destination_for(kind: AssetKind) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    match kind {
        AssetKind::Binary => Some(exe),
        AssetKind::AppBundle => walk_up_to_app(&exe),
        AssetKind::AppImage => Some(exe),
        AssetKind::Deb | AssetKind::Rpm | AssetKind::Nsis | AssetKind::Msi | AssetKind::Archive => {
            None
        }
    }
}

/// On macOS, the binary lives at `MyApp.app/Contents/MacOS/MyApp`. Walk up
/// the parent chain until we find the directory ending in `.app`.
fn walk_up_to_app(start: &std::path::Path) -> Option<PathBuf> {
    let mut current = start.parent();
    while let Some(dir) = current {
        if dir.extension().and_then(|s| s.to_str()) == Some("app") {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}
