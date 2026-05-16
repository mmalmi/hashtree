use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::{
    Config, FetchConfig, Fetcher, HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver,
};
use hashtree_core::{Hash, HashTree, HashTreeConfig, Store, StoreError};
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
        match self
            .fetcher
            .fetch_chunk_with_store(&self.store, None, hash)
            .await
        {
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
    let options = build_check_options(&reference, current_version.clone(), target, manifest_path)?;
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

use serde::{Deserialize, Serialize};

/// Cached result of the most recent background check. Written by the
/// detached `__bg_check` child, read on every startup to print the
/// pending notification before the new command runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CachedUpdateResult {
    /// Unix seconds when the check completed.
    checked_at: u64,
    /// htree version the check was run as.
    checked_as_version: String,
    /// Latest published htree version, if newer than `checked_as_version`.
    available_version: Option<String>,
    /// Whether the bg flow already installed the update (auto_install).
    installed: bool,
}

fn cached_result_path(data_dir: &Path) -> PathBuf {
    data_dir.join("last-update-result.json")
}

fn sentinel_path(data_dir: &Path) -> PathBuf {
    data_dir.join("last-update-check")
}

/// Read the cached result and print an inline notification if there's a
/// newer version than the running binary. Strictly bounded: the file
/// read happens on a worker thread with a 50 ms timeout so a hung
/// filesystem (NFS, FUSE, busy spinning disk) can't stall command
/// startup. Worst case the notification skips this round.
pub(crate) fn print_cached_update_notification(data_dir: &Path) {
    let path = cached_result_path(data_dir);
    let contents = match read_to_string_with_timeout(path, std::time::Duration::from_millis(50)) {
        Some(s) => s,
        None => return,
    };
    let Ok(cached): std::result::Result<CachedUpdateResult, _> = serde_json::from_str(&contents)
    else {
        return;
    };
    let Some(version) = cached.available_version.as_deref() else {
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    // Don't notify if the user already upgraded since the cache was written
    // (eg they ran `htree update` manually) or if the cached check was for
    // an older version of htree.
    if version == current {
        return;
    }
    if cached.installed {
        eprintln!(
            "htree was self-updated to {version} in the background — restart htree to use it",
        );
    } else {
        eprintln!(
            "htree update available: {version} (you're on {current}) — run `{}` to install",
            upgrade_command_hint(),
        );
    }
}

/// Read a file's contents on a worker thread, returning `None` if the
/// read doesn't complete within `timeout`. The worker is orphaned on
/// timeout — fine, the OS reaps it on process exit and the FS layer
/// will eventually unblock or the kernel will tear down the syscall.
fn read_to_string_with_timeout(path: PathBuf, timeout: std::time::Duration) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read_to_string(&path).ok());
    });
    rx.recv_timeout(timeout).ok().flatten()
}

/// Spawn a detached child process to run the actual check. Survives the
/// parent's exit (important for short commands like `htree user` whose
/// tokio runtime tears down before a network call would complete).
/// Throttled by mtime on the sentinel file.
pub(crate) fn spawn_detached_bg_check(data_dir: &Path) {
    let config = match Config::load() {
        Ok(c) => c,
        Err(_) => return,
    };
    if !config.updater.auto_check {
        return;
    }

    let interval = std::time::Duration::from_secs(
        u64::from(config.updater.check_interval_hours).saturating_mul(3600),
    );
    let sentinel = sentinel_path(data_dir);
    if let Ok(meta) = std::fs::metadata(&sentinel) {
        if let Ok(modified) = meta.modified() {
            if modified.elapsed().map(|e| e < interval).unwrap_or(false) {
                return;
            }
        }
    }
    // Touch the sentinel up-front so concurrent invocations don't all race
    // the same network request. Race window is small but the worst case is
    // two simultaneous checks once per interval — no correctness issue.
    if let Some(parent) = sentinel.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&sentinel)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    // Detached: stdio piped to /dev/null, parent doesn't await. The OS
    // reaps the orphaned child via init/launchd. Failure to spawn is fine
    // — we just won't get a notification this round.
    use std::process::Stdio;
    let _ = std::process::Command::new(exe)
        .arg("__bg_check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Body of the detached `htree __bg_check` invocation. Runs the check
/// (and optional install when auto_install is set), writes the result to
/// the cache file, exits.
pub(crate) async fn run_bg_check(data_dir: &Path) -> Result<()> {
    let config = Config::load().unwrap_or_default();
    let updater = build_updater(data_dir).await?;
    let options = build_check_options(
        super::args::HTREE_SELF_REFERENCE,
        env!("CARGO_PKG_VERSION").to_string(),
        None,
        "release.json".to_string(),
    )?;
    let mut cached = CachedUpdateResult {
        checked_at: now_unix(),
        checked_as_version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    };

    let check = match updater.check(options).await {
        Ok(c) => c,
        // Quiet for "no release yet" cases — write empty result so the
        // notifier stays silent.
        Err(hashtree_updater::UpdateError::ReleaseNotFound(_))
        | Err(hashtree_updater::UpdateError::ManifestNotFound(_)) => {
            write_cached_result(data_dir, &cached);
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    if !check.update_available {
        write_cached_result(data_dir, &cached);
        return Ok(());
    }
    let version = check.manifest.effective_version();
    cached.available_version = Some(version.clone());

    if config.updater.auto_install {
        let exe = std::env::current_exe()?;
        let mut asset = check
            .asset
            .clone()
            .context("no asset matched the platform")?;
        if asset.executable.is_none() {
            asset.executable = Some("htree".to_string());
        }
        let downloaded = updater
            .download(&check, DownloadOptions::default(), None)
            .await?;
        let target = InstallTarget::new(&exe).executable(true);
        if install(&asset, &downloaded.bytes, &target).is_ok() {
            cached.installed = true;
        }
    }

    write_cached_result(data_dir, &cached);
    Ok(())
}

fn write_cached_result(data_dir: &Path, cached: &CachedUpdateResult) {
    let path = cached_result_path(data_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cached) {
        let _ = std::fs::write(&path, json);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How the running htree binary was installed — used to print the right
/// upgrade hint when self-updating clashes with a package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSource {
    Cargo,
    Brew,
    Other,
}

fn detect_install_source() -> InstallSource {
    let Ok(exe) = std::env::current_exe() else {
        return InstallSource::Other;
    };
    let s = exe.to_string_lossy();
    if s.contains("/.cargo/bin/") {
        InstallSource::Cargo
    } else if s.contains("/Cellar/") || s.contains("/homebrew/") {
        InstallSource::Brew
    } else {
        InstallSource::Other
    }
}

/// The right one-liner to suggest in the update notification, picked
/// from the running binary's install source so the user can copy-paste it.
fn upgrade_command_hint() -> &'static str {
    match detect_install_source() {
        InstallSource::Cargo => "cargo install hashtree-cli --force",
        InstallSource::Brew => "brew upgrade htree",
        InstallSource::Other => "htree update",
    }
}

/// Self-update: install a fresh htree binary over the running one.
///
/// Refuses when the binary lives under a known package-manager path
/// (cargo, brew) so the package manager's metadata stays in sync. Pass
/// `force = true` to bypass and replace the binary directly anyway.
pub(crate) async fn run_self_update(data_dir: &Path, check_only: bool, force: bool) -> Result<()> {
    let current_exe = std::env::current_exe()?;
    if !check_only && !force {
        match detect_install_source() {
            InstallSource::Cargo => bail!(
                "htree was installed via cargo at {}.\n\
                 Run `cargo install hashtree-cli --force` to upgrade and keep cargo metadata in sync,\n\
                 or `htree update --force` to replace the binary directly anyway.",
                current_exe.display(),
            ),
            InstallSource::Brew => bail!(
                "htree was installed via brew at {}.\n\
                 Run `brew upgrade htree` to upgrade and keep brew metadata in sync,\n\
                 or `htree update --force` to replace the binary directly anyway.",
                current_exe.display(),
            ),
            InstallSource::Other => {}
        }
    }
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
