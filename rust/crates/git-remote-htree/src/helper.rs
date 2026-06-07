//! Git remote helper protocol implementation
//!
//! Implements the stateless git remote helper protocol.
//! See: https://git-scm.com/docs/gitremote-helpers

use crate::git::storage::GitStorage;
use crate::runtime::block_on_result;
use anyhow::{bail, Context, Result};
use hashtree_core::{Cid, Store};
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

mod cached_store;
mod git_objects;
mod progress;
mod push;
mod storage_support;

use progress::UploadProgress;
use storage_support::get_hashtree_data_dir;
#[cfg(test)]
use storage_support::{build_repo_viewer_url, queue_hash_if_new};

/// Threshold for showing detailed progress (3 seconds)
const VERBOSE_THRESHOLD: Duration = Duration::from_secs(3);
const DEFAULT_GIT_TREE_WALK_CONCURRENCY: usize = 4;
const MAX_GIT_TREE_WALK_CONCURRENCY: usize = 32;
const DEFAULT_GIT_OBJECT_DOWNLOAD_CONCURRENCY: usize = 64;
const DEFAULT_DIRECT_GIT_OBJECT_DOWNLOAD_CONCURRENCY: usize = 16;
const MAX_GIT_OBJECT_DOWNLOAD_CONCURRENCY: usize = 256;

use crate::nostr_client::NostrClient;
use hashtree_config::Config;

fn upload_progress(
    processed: usize,
    discovered: usize,
    total: Option<usize>,
    uploaded: usize,
    skipped_diff: usize,
    skipped_server: usize,
    failed: usize,
    has_old_tree: bool,
) -> UploadProgress {
    UploadProgress {
        processed,
        discovered,
        total,
        uploaded,
        skipped_diff,
        skipped_server,
        failed,
        has_old_tree,
    }
}

fn git_tree_walk_concurrency() -> usize {
    std::env::var("HTREE_GIT_TREE_WALK_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_GIT_TREE_WALK_CONCURRENCY))
        .unwrap_or(DEFAULT_GIT_TREE_WALK_CONCURRENCY)
}

fn configured_git_object_download_concurrency() -> Option<usize> {
    std::env::var("HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_GIT_OBJECT_DOWNLOAD_CONCURRENCY))
}

fn is_loopback_read_server(server: &str) -> bool {
    let lower = server.to_ascii_lowercase();
    let without_scheme = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .unwrap_or(&lower);
    let host_port_path = without_scheme.split('/').next().unwrap_or(without_scheme);
    let host = host_port_path
        .strip_prefix('[')
        .and_then(|value| value.split(']').next())
        .unwrap_or_else(|| host_port_path.split(':').next().unwrap_or(host_port_path));

    host == "localhost" || host == "::1" || host.starts_with("127.")
}

fn git_object_download_concurrency_for_read_servers(read_servers: &[String]) -> usize {
    if let Some(configured) = configured_git_object_download_concurrency() {
        return configured;
    }

    match read_servers {
        [] | [_] => DEFAULT_DIRECT_GIT_OBJECT_DOWNLOAD_CONCURRENCY,
        [first, ..] if is_loopback_read_server(first) => {
            DEFAULT_DIRECT_GIT_OBJECT_DOWNLOAD_CONCURRENCY
        }
        _ => DEFAULT_GIT_OBJECT_DOWNLOAD_CONCURRENCY,
    }
}

/// Git remote helper state machine
pub struct RemoteHelper {
    #[allow(dead_code)]
    pubkey: String,
    repo_name: String,
    storage: GitStorage,
    nostr: NostrClient,
    #[allow(dead_code)]
    config: Config,
    should_exit: bool,
    /// Refs advertised by remote
    remote_refs: HashMap<String, String>,
    /// Objects to push
    push_specs: Vec<PushSpec>,
    /// Objects to fetch
    fetch_specs: Vec<FetchSpec>,
    /// Secret key from URL fragment #k=<hex> (for link-visible repos)
    /// If set, use this for encryption instead of CHK, and don't publish key in event
    url_secret: Option<[u8; 32]>,
    /// Whether this is a private (author-only) repo using NIP-44 encryption
    is_private: bool,
    /// Start time for current operation (for conditional verbose logging)
    op_start: Option<Instant>,
}

#[derive(Debug)]
struct PushSpec {
    src: String, // local ref or sha
    dst: String, // remote ref
    force: bool,
}

#[derive(Debug)]
struct FetchSpec {
    sha: String,
    name: String,
}

#[derive(Debug, Clone)]
struct GitObjectLocation {
    oid: String,
    cid: Cid,
}

#[derive(Debug, Clone)]
struct GitPackLocation {
    pack_name: String,
    pack_cid: Cid,
    pack_size: u64,
    idx_name: String,
    idx_cid: Option<Cid>,
    idx_size: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct GitFetchStats {
    enumerated: usize,
    cached: usize,
    written: usize,
    enumerate_elapsed: Duration,
    local_check_elapsed: Duration,
    download_write_elapsed: Duration,
}

#[derive(Debug, PartialEq, Eq)]
enum AncestorCheck {
    /// Remote tip is an ancestor of local tip: fast-forward allowed.
    Ancestor,
    /// Remote tip is not an ancestor of local tip: true non-fast-forward.
    NotAncestor,
    /// We could not determine ancestry (merge-base command/object failure).
    Unknown(String),
}

fn is_safe_git_pack_name(name: &str) -> bool {
    name.len() == "pack-".len() + 40 + ".pack".len()
        && name.starts_with("pack-")
        && name.ends_with(".pack")
        && name["pack-".len()..name.len() - ".pack".len()]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

impl RemoteHelper {
    async fn with_git_pack_progress<T>(
        label: String,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc as StdArc;

        let done = StdArc::new(AtomicBool::new(false));
        let done_for_task = StdArc::clone(&done);
        let label_for_task = label.clone();
        eprint!("\r  {}...", label);
        let _ = std::io::stderr().flush();
        let progress_task = tokio::spawn(async move {
            let start = std::time::Instant::now();
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(750)).await;
                if done_for_task.load(Ordering::Relaxed) {
                    break;
                }
                eprint!(
                    "\r  {}... {:.1}s",
                    label_for_task,
                    start.elapsed().as_secs_f32()
                );
                let _ = std::io::stderr().flush();
            }
        });

        let result = future.await;
        done.store(true, Ordering::Relaxed);
        let _ = progress_task.await;
        result
    }

    fn format_transfer_bytes(bytes: u64) -> String {
        const KIB: f64 = 1024.0;
        const MIB: f64 = KIB * 1024.0;
        const GIB: f64 = MIB * 1024.0;

        let bytes = bytes as f64;
        if bytes >= GIB {
            format!("{:.1} GiB", bytes / GIB)
        } else if bytes >= MIB {
            format!("{:.1} MiB", bytes / MIB)
        } else if bytes >= KIB {
            format!("{:.1} KiB", bytes / KIB)
        } else {
            format!("{bytes:.0} B")
        }
    }

    fn is_repo_not_found_error(err: &anyhow::Error) -> bool {
        let message = err.to_string();
        message.starts_with("Repository '") && message.contains("' not found")
    }

    pub fn new(
        pubkey: &str,
        repo_name: &str,
        signing_key: Option<String>,
        url_secret: Option<[u8; 32]>,
        is_private: bool,
        config: Config,
    ) -> Result<Self> {
        // Use shared hashtree storage at ~/.hashtree/data
        let data_dir = get_hashtree_data_dir();
        debug!(?data_dir, "RemoteHelper::new");
        let storage = GitStorage::open(&data_dir)?;
        let nostr = NostrClient::new(pubkey, signing_key, url_secret, is_private, &config)?;

        if is_private {
            info!("Private repo: using NIP-44 encryption (author-only)");
        } else if url_secret.is_some() {
            info!("Link-visible repo: using secret from URL fragment");
        }

        Ok(Self {
            pubkey: pubkey.to_string(),
            repo_name: repo_name.to_string(),
            storage,
            nostr,
            config,
            should_exit: false,
            remote_refs: HashMap::new(),
            push_specs: Vec::new(),
            fetch_specs: Vec::new(),
            url_secret,
            is_private,
            op_start: None,
        })
    }

    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Start timing an operation (for conditional verbose logging)
    fn start_op(&mut self) {
        self.op_start = Some(Instant::now());
    }

    /// Check if operation has been running long enough to show details
    /// Also returns true if HTREE_VERBOSE=1 is set (for testing/debugging)
    fn is_slow(&self) -> bool {
        if std::env::var("HTREE_VERBOSE").is_ok() {
            return true;
        }
        self.op_start
            .map(|start| start.elapsed() >= VERBOSE_THRESHOLD)
            .unwrap_or(false)
    }

    /// Log detail message only if operation is slow
    fn detail(&self, msg: &str) {
        if self.is_slow() {
            eprintln!("{}", msg);
        }
    }

    /// Handle a single command from git
    pub fn handle_command(&mut self, line: &str) -> Result<Option<Vec<String>>> {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).copied();

        match cmd {
            "capabilities" => Ok(Some(self.capabilities())),
            "list" => {
                let for_push = arg == Some("for-push");
                self.list_refs(for_push)
            }
            "fetch" => {
                if let Some(arg) = arg {
                    self.queue_fetch(arg)?;
                }
                Ok(None)
            }
            "push" => {
                if let Some(arg) = arg {
                    self.queue_push(arg)?;
                }
                Ok(None)
            }
            "" => {
                // Empty line - execute queued operations
                if !self.fetch_specs.is_empty() {
                    self.execute_fetch()?;
                }
                if !self.push_specs.is_empty() {
                    return self.execute_push();
                }
                // Final empty line means exit
                self.should_exit = true;
                Ok(Some(vec![String::new()]))
            }
            "option" => {
                // Options like "option verbosity 1"
                if let Some(arg) = arg {
                    let mut parts = arg.split_whitespace();
                    let name = parts.next().unwrap_or("");
                    if name == "update-head-ok" {
                        return Ok(Some(vec!["ok".to_string()]));
                    }
                    if name == "progress" || name == "verbosity" {
                        return Ok(Some(vec!["ok".to_string()]));
                    }
                }
                debug!("Ignoring option: {:?}", arg);
                Ok(Some(vec!["unsupported".to_string()]))
            }
            _ => {
                warn!("Unknown command: {}", cmd);
                Ok(None)
            }
        }
    }

    /// Return supported capabilities
    fn capabilities(&self) -> Vec<String> {
        vec![
            "fetch".to_string(),
            "push".to_string(),
            "option".to_string(),
            String::new(), // Empty line terminates
        ]
    }

    /// List refs available on remote
    fn list_refs(&mut self, for_push: bool) -> Result<Option<Vec<String>>> {
        if for_push && self.config.blossom.force_upload {
            debug!("Returning empty refs for push because force_upload is enabled");
            self.remote_refs.clear();
            return Ok(Some(vec![String::new()]));
        }

        // Advertise refs for clone/pull and for ordinary pushes so Git can skip true no-ops.
        self.remote_refs.clear();
        let refs = match self.nostr.fetch_refs(&self.repo_name) {
            Ok(refs) => refs,
            Err(err) if for_push && Self::is_repo_not_found_error(&err) => {
                debug!("Repository not found during push ref advertisement; treating as empty");
                HashMap::new()
            }
            Err(err) => return Err(err),
        };

        let mut lines = Vec::new();

        for (name, sha) in &refs {
            self.remote_refs.insert(name.clone(), sha.clone());
            if name == "HEAD" {
                // HEAD can be a symref or a direct SHA.
                if let Some(target_branch) = sha.strip_prefix("ref: ") {
                    lines.push(format!("@{} HEAD", target_branch));
                } else {
                    lines.push(format!("{} HEAD", sha));
                }
            } else {
                lines.push(format!("{} {}", sha, name));
            }
        }

        // Empty repo
        if lines.is_empty() {
            debug!("Remote has no refs");
        }

        lines.push(String::new()); // Empty line terminates
        Ok(Some(lines))
    }

    /// Queue a fetch operation
    fn queue_fetch(&mut self, arg: &str) -> Result<()> {
        // Format: <sha> <name>
        let parts: Vec<&str> = arg.splitn(2, ' ').collect();
        if parts.len() != 2 {
            bail!("Invalid fetch spec: {}", arg);
        }

        self.fetch_specs.push(FetchSpec {
            sha: parts[0].to_string(),
            name: parts[1].to_string(),
        });
        Ok(())
    }

    /// Execute queued fetch operations
    fn execute_fetch(&mut self) -> Result<()> {
        self.start_op(); // Start timing for conditional verbose logging
        info!("Fetching {} refs", self.fetch_specs.len());
        for spec in &self.fetch_specs {
            debug!(sha = %spec.sha, name = %spec.name, "Queued fetch");
        }

        // Get the cached root hash from nostr (set during list command)
        let root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();

        if let Some(ref root) = root_hash {
            let stats = self.fetch_git_objects_to_local_git(root)?;
            info!(
                "Fetched {} git objects from hashtree ({} new, {} cached)",
                stats.enumerated, stats.written, stats.cached
            );

            if self.is_slow() {
                eprintln!(
                    "  Fetch stages: enumerate {:?}, local-check {:?}, download+write {:?}",
                    stats.enumerate_elapsed,
                    stats.local_check_elapsed,
                    stats.download_write_elapsed
                );
            }
        } else {
            bail!("No root hash found for repository - cannot fetch");
        }

        self.fetch_specs.clear();
        Ok(())
    }

    /// Fetch all git objects from hashtree's .git/objects/ directory
    fn fetch_all_git_objects(&self, root_hash: &str) -> Result<Vec<(String, Vec<u8>)>> {
        // NostrClient now handles unmasking for link-visible repos (url_secret)
        // The cached key is already the real CHK key
        let encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .cloned();

        info!(
            "fetch_all_git_objects: root={}, has encryption_key: {}, link_visible: {}",
            &root_hash[..12],
            encryption_key.is_some(),
            self.url_secret.is_some()
        );

        block_on_result(self.fetch_git_objects_async(root_hash, encryption_key.as_ref()))
    }

    fn fetch_git_objects_to_local_git(&self, root_hash: &str) -> Result<GitFetchStats> {
        let encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .cloned();

        info!(
            "fetch_git_objects_to_local_git: root={}, has encryption_key: {}, link_visible: {}",
            &root_hash[..12],
            encryption_key.is_some(),
            self.url_secret.is_some()
        );

        block_on_result(
            self.fetch_git_objects_to_local_git_async(root_hash, encryption_key.as_ref()),
        )
    }

    fn build_cached_fetch_tree(
        &self,
    ) -> Result<(
        hashtree_core::HashTree<cached_store::CachedStore>,
        std::sync::Arc<dyn Store + Send + Sync>,
    )> {
        use hashtree_blossom::BlossomStore;
        use hashtree_core::{HashTree, HashTreeConfig};

        let blossom = self.nostr.blossom();
        let servers = blossom.read_servers().to_vec();
        info!(
            "Creating CachedStore with local + Blossom (servers: {:?})",
            servers
        );

        let local_store: std::sync::Arc<dyn Store + Send + Sync> = self.storage.store().clone();
        let local_store_for_eviction = local_store.clone();

        let blossom_store = BlossomStore::with_servers(nostr::Keys::generate(), servers);

        let store = cached_store::CachedStore::new(local_store, blossom_store);
        let tree = HashTree::new(HashTreeConfig::new(std::sync::Arc::new(store)));
        Ok((tree, local_store_for_eviction))
    }

    async fn collect_git_object_locations_async(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<(
        hashtree_core::HashTree<cached_store::CachedStore>,
        Vec<GitObjectLocation>,
        Vec<GitPackLocation>,
        std::sync::Arc<dyn Store + Send + Sync>,
    )> {
        let (tree, local_store_for_eviction) = self.build_cached_fetch_tree()?;

        let root_bytes = hex::decode(root_hash).context("Invalid root hash hex")?;
        let root_arr: [u8; 32] = root_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Root hash must be 32 bytes"))?;

        let root_cid = Cid {
            hash: root_arr,
            key: encryption_key.copied(),
        };

        let objects_cid = match tree.resolve_path(&root_cid, ".git/objects").await {
            Ok(Some(cid)) => cid,
            Ok(None) => {
                warn!("No .git/objects directory found");
                return Ok((tree, Vec::new(), Vec::new(), local_store_for_eviction));
            }
            Err(e) => {
                warn!("Failed to resolve .git/objects: {}", e);
                return Ok((tree, Vec::new(), Vec::new(), local_store_for_eviction));
            }
        };

        info!("Resolved .git/objects: {}", hex::encode(objects_cid.hash));
        let pack_locations = self
            .collect_git_pack_locations_async(&tree, &objects_cid)
            .await?;

        use hashtree_core::LinkType;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        let progress = StdArc::new(AtomicUsize::new(0));
        let done = StdArc::new(AtomicBool::new(false));

        let progress_clone = progress.clone();
        let done_clone = done.clone();
        let progress_task = tokio::spawn(async move {
            let mut last = 0;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                if done_clone.load(Ordering::Relaxed) {
                    break;
                }
                let current = progress_clone.load(Ordering::Relaxed);
                if current != last {
                    eprint!("\r  Loading objects tree... {} nodes", current);
                    let _ = std::io::stderr().flush();
                    last = current;
                }
            }
        });

        let walk_entries = match tree
            .walk_parallel_with_progress(
                &objects_cid,
                "",
                git_tree_walk_concurrency(),
                Some(&progress),
            )
            .await
        {
            Ok(entries) => entries,
            Err(e) => {
                done.store(true, Ordering::Relaxed);
                let _ = progress_task.await;
                eprintln!("\r  Loading objects tree... failed: {}", e);
                warn!("Failed to walk objects directory: {}", e);
                return Ok((tree, Vec::new(), pack_locations, local_store_for_eviction));
            }
        };
        done.store(true, Ordering::Relaxed);
        let _ = progress_task.await;

        if self.is_slow() {
            eprintln!(
                "\r  Loading objects tree... done ({} entries)        ",
                walk_entries.len()
            );
        } else {
            eprint!("\r                                                        \r");
        }

        let mut fetch_tasks: Vec<GitObjectLocation> = Vec::new();
        for entry in walk_entries {
            if entry.link_type == LinkType::Dir {
                continue;
            }

            let parts: Vec<&str> = entry.path.split('/').collect();
            if parts.len() == 2 && parts[0].len() == 2 && parts[1].len() == 38 {
                if hex::decode(parts[0]).is_ok() && hex::decode(parts[1]).is_ok() {
                    fetch_tasks.push(GitObjectLocation {
                        oid: format!("{}{}", parts[0], parts[1]),
                        cid: Cid {
                            hash: entry.hash,
                            key: entry.key,
                        },
                    });
                }
            } else if parts.len() == 1 && parts[0].len() == 40 && hex::decode(parts[0]).is_ok() {
                fetch_tasks.push(GitObjectLocation {
                    oid: parts[0].to_string(),
                    cid: Cid {
                        hash: entry.hash,
                        key: entry.key,
                    },
                });
            }
        }

        Ok((tree, fetch_tasks, pack_locations, local_store_for_eviction))
    }

    async fn collect_git_pack_locations_async<S: Store>(
        &self,
        tree: &hashtree_core::HashTree<S>,
        objects_cid: &Cid,
    ) -> Result<Vec<GitPackLocation>> {
        let Some(info_packs_cid) = tree
            .resolve_path(objects_cid, "info/packs")
            .await
            .context("resolve .git/objects/info/packs")?
        else {
            return Ok(Vec::new());
        };

        let Some(info_packs_bytes) = tree
            .get(&info_packs_cid, None)
            .await
            .context("read .git/objects/info/packs")?
        else {
            return Ok(Vec::new());
        };

        let Some(pack_dir_cid) = tree
            .resolve_path(objects_cid, "pack")
            .await
            .context("resolve .git/objects/pack")?
        else {
            return Ok(Vec::new());
        };
        let pack_entries: HashMap<_, _> = tree
            .list_directory(&pack_dir_cid)
            .await
            .context("list .git/objects/pack")?
            .into_iter()
            .map(|entry| (entry.name.clone(), entry))
            .collect();

        let info_packs = String::from_utf8_lossy(&info_packs_bytes);
        let mut packs = Vec::new();
        for line in info_packs.lines().map(str::trim) {
            let Some(pack_name) = line.strip_prefix("P ") else {
                continue;
            };
            if !is_safe_git_pack_name(pack_name) {
                continue;
            }

            let Some(pack_entry) = pack_entries.get(pack_name) else {
                continue;
            };
            let pack_cid = Cid {
                hash: pack_entry.hash,
                key: pack_entry.key,
            };

            let idx_name = format!("{}.idx", pack_name.trim_end_matches(".pack"));
            let idx_entry = pack_entries.get(&idx_name);
            let idx_cid = idx_entry.map(|entry| Cid {
                hash: entry.hash,
                key: entry.key,
            });

            packs.push(GitPackLocation {
                pack_name: pack_name.to_string(),
                pack_cid,
                pack_size: pack_entry.size,
                idx_name,
                idx_cid,
                idx_size: idx_entry.map(|entry| entry.size),
            });
        }

        Ok(packs)
    }

    async fn stream_git_pack_file<S: Store>(
        tree: &hashtree_core::HashTree<S>,
        cid: &Cid,
        destination: &Path,
        label: String,
        expected_size: Option<u64>,
    ) -> Result<u64> {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create {}", parent.display()))?;
        }

        let temp_name = format!(
            ".{}.{}.tmp",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("pack"),
            std::process::id()
        );
        let temp_path = destination.with_file_name(temp_name);
        let _ = tokio::fs::remove_file(&temp_path).await;

        let mut file = tokio::fs::File::create(&temp_path)
            .await
            .with_context(|| format!("create {}", temp_path.display()))?;
        let mut stream = tree.get_stream(cid);
        let mut written = 0u64;
        let mut saw_chunk = false;
        let mut last_progress = Instant::now() - Duration::from_secs(1);

        eprint!("\r  {}... 0 B", label);
        let _ = std::io::stderr().flush();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("stream {}", destination.display()))?;
            saw_chunk = true;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("write {}", temp_path.display()))?;
            written += chunk.len() as u64;

            if last_progress.elapsed() >= Duration::from_millis(250) {
                Self::print_pack_byte_progress(&label, written, expected_size);
                last_progress = Instant::now();
            }
        }

        if !saw_chunk {
            drop(file);
            let _ = tokio::fs::remove_file(&temp_path).await;
            bail!("{} was not found", label);
        }

        file.flush()
            .await
            .with_context(|| format!("flush {}", temp_path.display()))?;
        drop(file);

        if let Some(expected) = expected_size {
            if expected != written {
                let _ = tokio::fs::remove_file(&temp_path).await;
                bail!(
                    "{} size mismatch: expected {}, wrote {}",
                    label,
                    expected,
                    written
                );
            }
        }

        tokio::fs::rename(&temp_path, destination)
            .await
            .with_context(|| {
                format!(
                    "rename {} to {}",
                    temp_path.display(),
                    destination.display()
                )
            })?;
        Self::print_pack_byte_progress(&label, written, expected_size);
        Ok(written)
    }

    fn print_pack_byte_progress(label: &str, written: u64, expected_size: Option<u64>) {
        if let Some(expected) = expected_size {
            eprint!(
                "\r  {}... {}/{}",
                label,
                Self::format_transfer_bytes(written),
                Self::format_transfer_bytes(expected)
            );
        } else {
            eprint!("\r  {}... {}", label, Self::format_transfer_bytes(written));
        }
        let _ = std::io::stderr().flush();
    }

    async fn install_git_pack_files_async<S: Store>(
        &self,
        tree: &hashtree_core::HashTree<S>,
        pack_locations: &[GitPackLocation],
    ) -> Result<usize> {
        if pack_locations.is_empty() {
            return Ok(0);
        }

        let git_pack_dir = Self::git_dir_path().join("objects").join("pack");
        std::fs::create_dir_all(&git_pack_dir)
            .with_context(|| format!("create {}", git_pack_dir.display()))?;
        let mut installed = 0usize;
        for (index, location) in pack_locations.iter().enumerate() {
            let pack_path = git_pack_dir.join(&location.pack_name);
            let idx_path = git_pack_dir.join(&location.idx_name);
            if pack_path.exists() && idx_path.exists() {
                continue;
            }

            let pack_size = if pack_path.exists() {
                std::fs::metadata(&pack_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(location.pack_size)
            } else {
                let pack_label = format!(
                    "Downloading git pack {}/{} {}",
                    index + 1,
                    pack_locations.len(),
                    location.pack_name
                );
                Self::stream_git_pack_file(
                    tree,
                    &location.pack_cid,
                    &pack_path,
                    pack_label,
                    Some(location.pack_size),
                )
                .await
                .with_context(|| format!("read {}", location.pack_name))?
            };

            if let Some(idx_cid) = &location.idx_cid {
                let idx_label = format!(
                    "Downloading git pack index {}/{} {}",
                    index + 1,
                    pack_locations.len(),
                    location.idx_name
                );
                if !idx_path.exists() {
                    Self::stream_git_pack_file(
                        tree,
                        idx_cid,
                        &idx_path,
                        idx_label,
                        location.idx_size,
                    )
                    .await
                    .with_context(|| format!("read {}", location.idx_name))?;
                }
            }

            if !idx_path.exists() {
                let index_label = format!(
                    "Indexing git pack {}/{} {}",
                    index + 1,
                    pack_locations.len(),
                    location.pack_name
                );
                let status = Self::with_git_pack_progress(index_label, async {
                    Command::new("git")
                        .arg("index-pack")
                        .arg(&pack_path)
                        .status()
                })
                .await
                .context("run git index-pack")?;
                if !status.success() {
                    bail!("git index-pack failed for {}", pack_path.display());
                }
            }

            eprintln!(
                "\r  Installed git pack {}/{} {} ({})        ",
                index + 1,
                pack_locations.len(),
                location.pack_name,
                Self::format_transfer_bytes(pack_size)
            );
            installed += 1;
        }

        Ok(installed)
    }

    /// Async implementation of git object fetching using HashTree helpers
    async fn fetch_git_objects_async(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut objects = Vec::new();
        let (tree, fetch_tasks, _pack_locations, local_store_for_eviction) = self
            .collect_git_object_locations_async(root_hash, encryption_key)
            .await?;
        use futures::stream::{self, StreamExt};
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        let total_objects = fetch_tasks.len();

        let downloaded = StdArc::new(AtomicUsize::new(0));
        let download_done = StdArc::new(AtomicBool::new(false));

        // Spawn progress reporter
        let downloaded_clone = downloaded.clone();
        let download_done_clone = download_done.clone();
        let total_for_timer = total_objects;
        let timer_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                if download_done_clone.load(Ordering::Relaxed) {
                    break;
                }
                let count = downloaded_clone.load(Ordering::Relaxed);
                eprint!("\r  Loading: {}/{}    ", count, total_for_timer);
                let _ = std::io::stderr().flush();
            }
        });

        // Parallel fetch with concurrency limit
        let concurrency =
            git_object_download_concurrency_for_read_servers(self.nostr.blossom().read_servers());
        type FetchObjectResult = std::result::Result<(String, Vec<u8>), (String, Cid)>;

        // First pass: fetch all objects with normal timeout
        let results: Vec<FetchObjectResult> = stream::iter(fetch_tasks)
            .map(|location| {
                let tree = &tree;
                let downloaded = StdArc::clone(&downloaded);
                async move {
                    let result = match tree.get(&location.cid, None).await {
                        Ok(Some(content)) => Ok((location.oid, content)),
                        Ok(None) => Err((location.oid, location.cid)),
                        Err(_) => Err((location.oid, location.cid)),
                    };
                    downloaded.fetch_add(1, Ordering::Relaxed);
                    result
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

        download_done.store(true, Ordering::Relaxed);
        let _ = timer_task.await;

        // Collect successes and failures
        let mut failed: Vec<(String, Cid)> = Vec::new();
        for result in results {
            match result {
                Ok((oid, content)) => objects.push((oid, content)),
                Err((oid, cid)) => failed.push((oid, cid)),
            }
        }

        let success_count = objects.len();
        eprintln!("\r  Loading: {}/{}    ", success_count, total_objects);

        // Retry failed downloads sequentially
        let mut missing_objects: Vec<(String, String)> = Vec::new(); // (oid, hash)
        if !failed.is_empty() {
            eprintln!("  Retrying {} failed downloads...", failed.len());
            for (i, (oid, obj_cid)) in failed.iter().enumerate() {
                let hash_hex = hex::encode(obj_cid.hash);
                eprint!("\r  Retrying {}/{}: {}...    ", i + 1, failed.len(), oid);
                let _ = std::io::stderr().flush();

                match tree.get(obj_cid, None).await {
                    Ok(Some(content)) => {
                        objects.push((oid.clone(), content));
                    }
                    Ok(None) => {
                        eprintln!("\n  ERROR: Object {} not found (hash: {})", oid, hash_hex);
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                    Err(e) => {
                        eprintln!(
                            "\n  ERROR: Failed to fetch {}: {} (hash: {})",
                            oid, e, hash_hex
                        );
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                }
            }
            eprintln!(
                "\r  Retried: {}/{} objects available        ",
                objects.len(),
                total_objects
            );
        }

        // Fail if any objects are missing - git clone will fail anyway
        if !missing_objects.is_empty() {
            let obj_list: Vec<String> = missing_objects
                .iter()
                .take(5)
                .map(|(oid, hash)| format!("{} ({})", oid, hash))
                .collect();
            bail!(
                "Failed to fetch {} required git objects:\n  {}",
                missing_objects.len(),
                obj_list.join("\n  ")
            );
        }

        info!("Fetched {} git objects from hashtree", objects.len());
        match local_store_for_eviction.evict_if_needed().await {
            Ok(freed) if freed > 0 => {
                info!(
                    "Evicted {} bytes from shared git blob cache after fetch",
                    freed
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to evict shared git blob cache after fetch: {}", err);
            }
        }
        Ok(objects)
    }

    async fn fetch_git_objects_to_local_git_async(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<GitFetchStats> {
        use futures::stream::{self, StreamExt};
        use std::io::Write;
        use tokio::sync::mpsc;

        let enumerate_start = std::time::Instant::now();
        let (tree, fetch_tasks, pack_locations, local_store_for_eviction) = self
            .collect_git_object_locations_async(root_hash, encryption_key)
            .await?;
        let enumerate_elapsed = enumerate_start.elapsed();

        let total_objects = fetch_tasks.len();
        if self.is_slow() {
            eprintln!(
                "  Prepared {} objects in {:?}",
                total_objects, enumerate_elapsed
            );
        }

        if !pack_locations.is_empty() {
            let pack_start = std::time::Instant::now();
            match self
                .install_git_pack_files_async(&tree, &pack_locations)
                .await
            {
                Ok(installed) if installed > 0 => {
                    eprintln!(
                        "  Installed {} git pack(s) in {:?}",
                        installed,
                        pack_start.elapsed()
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to install git pack checkpoint: {}", err);
                    if self.is_slow() {
                        eprintln!("  Warning: git pack checkpoint install failed: {}", err);
                    }
                }
            }
        }

        let local_check_start = std::time::Instant::now();
        let existing =
            self.git_batch_check_objects(fetch_tasks.iter().map(|location| location.oid.as_str()))?;
        let local_check_elapsed = local_check_start.elapsed();

        let pending: Vec<GitObjectLocation> = fetch_tasks
            .into_iter()
            .filter(|location| !existing.contains(&location.oid))
            .collect();
        let total_to_write = pending.len();
        let cached = existing.len();

        if total_to_write == 0 {
            eprintln!("  Writing to .git: 0 new, {} cached    ", cached);
            match local_store_for_eviction.evict_if_needed().await {
                Ok(freed) if freed > 0 => {
                    info!(
                        "Evicted {} bytes from shared git blob cache after fetch",
                        freed
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to evict shared git blob cache after fetch: {}", err);
                }
            }
            return Ok(GitFetchStats {
                enumerated: total_objects,
                cached,
                written: 0,
                enumerate_elapsed,
                local_check_elapsed,
                download_write_elapsed: Duration::default(),
            });
        }

        let transfer_start = std::time::Instant::now();
        let mut completed = 0usize;
        let mut queued_writes = 0usize;
        let mut failed: Vec<(String, Cid)> = Vec::new();

        let concurrency =
            git_object_download_concurrency_for_read_servers(self.nostr.blossom().read_servers());
        const WRITE_QUEUE_CAPACITY: usize = 256;
        let git_dir = Self::git_dir_path();
        let (write_tx, mut write_rx) = mpsc::channel::<(String, Vec<u8>)>(WRITE_QUEUE_CAPACITY);
        let writer_task = tokio::spawn(async move {
            let mut written = 0usize;
            while let Some((oid, content)) = write_rx.recv().await {
                let writer_git_dir = git_dir.clone();
                tokio::task::spawn_blocking(move || {
                    Self::write_git_object_to_dir(&writer_git_dir, &oid, &content)
                })
                .await
                .context("git object writer task panicked")??;
                written += 1;
            }
            Ok::<usize, anyhow::Error>(written)
        });

        let mut results = stream::iter(pending.into_iter().map(|location| {
            let tree_ref = &tree;
            async move {
                match tree_ref.get(&location.cid, None).await {
                    Ok(Some(content)) => Ok((location.oid, content)),
                    Ok(None) => Err((location.oid, location.cid)),
                    Err(_) => Err((location.oid, location.cid)),
                }
            }
        }))
        .buffer_unordered(concurrency);

        while let Some(result) = results.next().await {
            completed += 1;
            match result {
                Ok((oid, content)) => {
                    write_tx
                        .send((oid, content))
                        .await
                        .map_err(|_| anyhow::anyhow!("git object writer stopped unexpectedly"))?;
                    queued_writes += 1;
                }
                Err((oid, cid)) => failed.push((oid, cid)),
            }

            if completed == 1 || completed.is_multiple_of(50) || completed == total_to_write {
                eprint!("\r  Writing to .git: {}/{}    ", completed, total_to_write);
                let _ = std::io::stderr().flush();
            }
        }

        let mut missing_objects: Vec<(String, String)> = Vec::new();
        if !failed.is_empty() {
            eprintln!("\n  Retrying {} failed downloads...", failed.len());
            for (i, (oid, obj_cid)) in failed.iter().enumerate() {
                let hash_hex = hex::encode(obj_cid.hash);
                eprint!("\r  Retrying {}/{}: {}...    ", i + 1, failed.len(), oid);
                let _ = std::io::stderr().flush();

                match tree.get(obj_cid, None).await {
                    Ok(Some(content)) => {
                        write_tx.send((oid.clone(), content)).await.map_err(|_| {
                            anyhow::anyhow!("git object writer stopped unexpectedly")
                        })?;
                        queued_writes += 1;
                    }
                    Ok(None) => {
                        eprintln!("\n  ERROR: Object {} not found (hash: {})", oid, hash_hex);
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                    Err(e) => {
                        eprintln!(
                            "\n  ERROR: Failed to fetch {}: {} (hash: {})",
                            oid, e, hash_hex
                        );
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                }
            }
            eprintln!(
                "\r  Retried: {}/{} objects available        ",
                queued_writes, total_to_write
            );
        }

        drop(write_tx);
        let written = writer_task
            .await
            .context("failed to join git object writer task")??;

        if !missing_objects.is_empty() {
            let obj_list: Vec<String> = missing_objects
                .iter()
                .take(5)
                .map(|(oid, hash)| format!("{} ({})", oid, hash))
                .collect();
            bail!(
                "Failed to fetch {} required git objects:\n  {}",
                missing_objects.len(),
                obj_list.join("\n  ")
            );
        }

        if cached > 0 {
            eprintln!(
                "\r  Writing to .git: {} new, {} cached    ",
                written, cached
            );
        } else {
            eprintln!("\r  Writing to .git: {}/{}    ", written, written);
        }

        let download_write_elapsed = transfer_start.elapsed();
        match local_store_for_eviction.evict_if_needed().await {
            Ok(freed) if freed > 0 => {
                info!(
                    "Evicted {} bytes from shared git blob cache after fetch",
                    freed
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to evict shared git blob cache after fetch: {}", err);
            }
        }

        Ok(GitFetchStats {
            enumerated: total_objects,
            cached,
            written,
            enumerate_elapsed,
            local_check_elapsed,
            download_write_elapsed,
        })
    }
}

#[cfg(test)]
mod tests;
