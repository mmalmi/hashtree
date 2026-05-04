use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::{
    Config, FetchConfig, Fetcher, HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver,
};
use hashtree_core::{HashTree, HashTreeConfig, Hash, Store, StoreError};
use hashtree_updater::{
    install, AssetKind, DownloadEvent, DownloadOptions, HashtreeUpdater, InstallTarget,
    UpdateAsset, UpdateCheckOptions, UpdateRef, UpdateTarget,
};

/// `Store` adapter that backs reads with a `Fetcher` so unknown chunks are
/// pulled from Blossom/WebRTC on demand. Writes pass straight through to the
/// underlying `HashtreeStore`.
struct FetchingStore {
    store: Arc<HashtreeStore>,
    fetcher: Arc<Fetcher>,
}

impl FetchingStore {
    fn new(store: Arc<HashtreeStore>, fetcher: Arc<Fetcher>) -> Self {
        Self { store, fetcher }
    }
}

#[async_trait]
impl Store for FetchingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.store.store_arc().put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.store.store_arc().put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.store.store_arc().get(hash).await? {
            return Ok(Some(data));
        }
        match self.fetcher.fetch_chunk_with_store(&self.store, None, hash).await {
            Ok(data) => Ok(Some(data)),
            Err(_) => Ok(None),
        }
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.store.store_arc().has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.store.store_arc().delete(hash).await
    }
}

async fn build_updater(
    data_dir: &Path,
) -> Result<HashtreeUpdater<NostrRootResolver, FetchingStore>> {
    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let fetcher = Arc::new(Fetcher::new(FetchConfig::default()));
    let fetching_store = Arc::new(FetchingStore::new(store, fetcher));
    let tree = HashTree::new(HashTreeConfig::new(fetching_store));

    let config = Config::load()?;
    let (nsec_str, _) = ensure_keys_string()?;
    let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
    let resolver_config = NostrResolverConfig {
        relays: config.nostr.relays.clone(),
        resolve_timeout: Duration::from_secs(10),
        secret_key: Some(keys),
    };
    let resolver = NostrRootResolver::new(resolver_config)
        .await
        .context("Failed to create Nostr resolver")?;
    Ok(HashtreeUpdater::new(resolver, tree))
}

fn build_check_options(
    reference: &str,
    current_version: String,
    target: Option<String>,
    manifest_path: String,
) -> Result<UpdateCheckOptions> {
    let reference = UpdateRef::parse(reference)?;
    let target = target
        .map(UpdateTarget::new)
        .unwrap_or_else(UpdateTarget::current);
    Ok(UpdateCheckOptions {
        reference,
        current_version,
        target,
        manifest_path,
        ..UpdateCheckOptions::default()
    })
}

/// Default destination for plain binaries / binary-archives when the user
/// doesn't pass --to. Picks `~/.local/bin/<asset binary name>`.
fn default_install_path(asset: &UpdateAsset) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set; pass --to explicitly")?;
    let bin_dir = PathBuf::from(home).join(".local/bin");
    let entry_name = asset
        .executable
        .as_deref()
        .map(|s| s.rsplit('/').next().unwrap_or(s))
        .unwrap_or_else(|| {
            // Strip common archive extensions to derive a sensible binary
            // name from the asset name itself.
            let n = asset.name.as_str();
            let n = n.strip_suffix(".tar.gz").unwrap_or(n);
            let n = n.strip_suffix(".tgz").unwrap_or(n);
            let n = n.strip_suffix(".zip").unwrap_or(n);
            n
        });
    Ok(bin_dir.join(entry_name))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_install(
    data_dir: &Path,
    reference: String,
    to: Option<PathBuf>,
    check_only: bool,
    download_only: bool,
    current_version: String,
    target: Option<String>,
    manifest_path: String,
    kind: Option<String>,
    executable: bool,
    archive_entry: Option<String>,
    only_if_newer: bool,
) -> Result<()> {
    let updater = build_updater(data_dir).await?;
    let options =
        build_check_options(&reference, current_version.clone(), target, manifest_path)?;
    let check = updater.check(options).await?;

    let mut asset: UpdateAsset = check
        .asset
        .clone()
        .context("no asset matched the platform")?;
    if let Some(kind_override) = kind {
        if AssetKind::parse(&kind_override).is_none() {
            bail!("unknown asset kind: {kind_override}");
        }
        asset.kind = Some(kind_override);
    }
    if let Some(entry) = archive_entry {
        asset.executable = Some(entry);
    }

    if check_only {
        println!("Version:    {}", check.manifest.effective_version());
        println!("Current:    {}", current_version);
        println!("Newer:      {}", check.update_available);
        if let Some(notes) = check.manifest.notes.as_deref() {
            println!("Notes:      {}", notes);
        }
        println!("Asset:      {} ({})", asset.name, asset.path);
        println!("Kind:       {}", asset.asset_kind().as_str());
        if let Some(exe) = asset.executable.as_deref() {
            println!("Entry:      {}", exe);
        }
        return Ok(());
    }

    if only_if_newer && !check.update_available {
        println!(
            "Already up to date (manifest version {} not newer)",
            check.manifest.effective_version()
        );
        return Ok(());
    }

    let downloaded = updater
        .download(&check, DownloadOptions::default(), Some(progress_logger()))
        .await?;

    if download_only {
        let out_path = to.unwrap_or_else(|| PathBuf::from(&asset.name));
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&out_path, &downloaded.bytes)?;
        println!(
            "Wrote {} bytes to {}",
            downloaded.bytes.len(),
            out_path.display()
        );
        return Ok(());
    }

    let dest = match to {
        Some(p) => p,
        None => default_install_path(&asset)?,
    };
    let target = InstallTarget::new(&dest).executable(executable);
    install(&asset, &downloaded.bytes, &target)?;
    println!(
        "Installed {} ({}) → {}",
        check.manifest.effective_version(),
        asset.asset_kind().as_str(),
        dest.display()
    );
    Ok(())
}

/// Self-update: install a fresh htree binary over the running one.
pub(crate) async fn run_self_update(data_dir: &Path, check_only: bool) -> Result<()> {
    let current_exe = std::env::current_exe()?;
    run_install(
        data_dir,
        super::args::HTREE_SELF_REFERENCE.to_string(),
        Some(current_exe),
        check_only,
        false,
        env!("CARGO_PKG_VERSION").to_string(),
        None,
        "release.json".to_string(),
        None,
        true,
        Some("htree".to_string()),
        true,
    )
    .await
}

fn progress_logger() -> hashtree_updater::DownloadCallback {
    Arc::new(|event| match event {
        DownloadEvent::Started { content_length } => {
            if let Some(total) = content_length {
                eprintln!("Downloading {} bytes...", total);
            } else {
                eprintln!("Downloading...");
            }
        }
        DownloadEvent::Progress {
            chunk_len: _,
            downloaded,
        } => {
            eprint!("\r  {} bytes", downloaded);
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
        DownloadEvent::Finished { total } => {
            eprintln!("\rdone ({} bytes)         ", total);
        }
    })
}
