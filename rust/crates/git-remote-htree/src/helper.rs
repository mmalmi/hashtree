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
use std::io::{IsTerminal, Write};
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
const DEFAULT_FETCH_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_GIT_PACK_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
const VERBOSE_FETCH_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const GIT_PACK_PHASE_IDLE: usize = 0;
const GIT_PACK_PHASE_DOWNLOADING: usize = 1;
const GIT_PACK_PHASE_INDEXING: usize = 2;

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

fn git_object_download_concurrency_for_read_servers(read_servers: &[String]) -> usize {
    if let Some(configured) = configured_git_object_download_concurrency() {
        return configured;
    }

    match read_servers {
        [] | [_] => DEFAULT_DIRECT_GIT_OBJECT_DOWNLOAD_CONCURRENCY,
        _ => DEFAULT_GIT_OBJECT_DOWNLOAD_CONCURRENCY,
    }
}

fn fetch_progress_interval() -> Duration {
    if std::env::var("HTREE_VERBOSE").is_ok() {
        VERBOSE_FETCH_PROGRESS_INTERVAL
    } else {
        DEFAULT_FETCH_PROGRESS_INTERVAL
    }
}

fn should_retry_local_daemon_fetch_failure(
    root_is_from_local_daemon: bool,
    local_daemon_only: bool,
) -> bool {
    root_is_from_local_daemon && !local_daemon_only
}

fn git_pack_progress_interval(stderr_is_terminal: bool) -> Duration {
    if stderr_is_terminal || std::env::var("HTREE_VERBOSE").is_ok() {
        VERBOSE_FETCH_PROGRESS_INTERVAL
    } else {
        DEFAULT_GIT_PACK_PROGRESS_INTERVAL
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
    /// Error from `list for-push` ref advertisement, if the remote root existed
    /// but could not be read before Git sent the actual push specs.
    push_ref_advertisement_error: Option<String>,
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

fn is_git_hex_name(name: &str, len: usize) -> bool {
    name.len() == len && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl RemoteHelper {
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

    fn format_git_pack_progress_line(
        processed_packs: usize,
        total_packs: usize,
        loaded_pack_bytes: u64,
        total_pack_bytes: u64,
        current_pack: usize,
        phase: usize,
        done: bool,
        elapsed: Duration,
    ) -> String {
        let loaded_pack_bytes = loaded_pack_bytes.min(total_pack_bytes);
        let progress = format!(
            "{}/{} ({}/{})",
            processed_packs,
            total_packs,
            Self::format_transfer_bytes(loaded_pack_bytes),
            Self::format_transfer_bytes(total_pack_bytes)
        );

        if done {
            return format!(
                "  Loading git packs: {} done in {:.1}s",
                progress,
                elapsed.as_secs_f32()
            );
        }

        let phase = match phase {
            GIT_PACK_PHASE_DOWNLOADING => {
                format!(", downloading {}/{}", current_pack, total_packs)
            }
            GIT_PACK_PHASE_INDEXING => format!(", indexing {}/{}", current_pack, total_packs),
            _ => String::new(),
        };
        let elapsed = if elapsed >= Duration::from_secs(1) {
            format!(", {:.0}s", elapsed.as_secs_f32())
        } else {
            String::new()
        };
        format!("  Loading git packs: {progress}{phase}{elapsed}")
    }

    fn emit_git_pack_progress_line(line: &str, same_line: bool, finish: bool) {
        if same_line {
            eprint!("\r{line}\x1b[K");
            if finish {
                eprintln!();
            }
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
    }

    fn is_repo_not_found_error(err: &anyhow::Error) -> bool {
        let message = err.to_string();
        message.starts_with("Repository '") && message.contains("' not found")
    }

    fn is_missing_root_download_error(message: &str) -> bool {
        if !message.contains("Failed to download root hash") {
            return false;
        }

        let lower = message.to_ascii_lowercase();
        message.contains("404") || lower.contains("not found")
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
            push_ref_advertisement_error: None,
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
            self.push_ref_advertisement_error = None;
            return Ok(Some(vec![String::new()]));
        }

        // Advertise refs for clone/pull and for ordinary pushes so Git can skip true no-ops.
        self.remote_refs.clear();
        let refs = match self.nostr.fetch_refs(&self.repo_name) {
            Ok(refs) => {
                if for_push {
                    self.push_ref_advertisement_error = None;
                }
                refs
            }
            Err(err) if for_push && Self::is_repo_not_found_error(&err) => {
                debug!("Repository not found during push ref advertisement; treating as empty");
                self.push_ref_advertisement_error = None;
                HashMap::new()
            }
            Err(err) if for_push => {
                let mut message = err.to_string();
                if Self::is_missing_root_download_error(&message) {
                    match self.reupload_cached_remote_root_after_missing_download(&message) {
                        Ok(true) => match self.nostr.fetch_refs(&self.repo_name) {
                            Ok(refs) => {
                                self.push_ref_advertisement_error = None;
                                refs
                            }
                            Err(retry_err) => {
                                message = retry_err.to_string();
                                warn!(
                                    "Could not read remote refs after root reupload repair: {}",
                                    message
                                );
                                self.push_ref_advertisement_error = Some(message.clone());
                                HashMap::new()
                            }
                        },
                        Ok(false) => {
                            self.push_ref_advertisement_error = Some(message.clone());
                            HashMap::new()
                        }
                        Err(repair_err) => {
                            eprintln!(
                                "  Warning: Could not reupload missing htree root from local store: {}",
                                repair_err
                            );
                            message = repair_err.to_string();
                            self.push_ref_advertisement_error = Some(message.clone());
                            HashMap::new()
                        }
                    }
                } else {
                    warn!(
                        "Could not read remote refs during push advertisement: {}",
                        message
                    );
                    eprintln!(
                        "  Warning: Could not read existing htree remote refs before push: {}",
                        message
                    );
                    eprintln!(
                        "  Ordinary pushes will be rejected unless remote state can be loaded; use --force only for explicit repair."
                    );
                    self.push_ref_advertisement_error = Some(message);
                    HashMap::new()
                }
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
            let stats = match self.fetch_git_objects_to_local_git(root) {
                Ok(stats) => stats,
                Err(err)
                    if self.nostr.cached_root_is_from_local_daemon(&self.repo_name)
                        && self.nostr.local_daemon_only() =>
                {
                    return Err(err).with_context(|| {
                        format!(
                            "local-daemon-only object fetch for {} failed; relay/Blossom fallback disabled",
                            self.repo_name
                        )
                    });
                }
                Err(err)
                    if should_retry_local_daemon_fetch_failure(
                        self.nostr.cached_root_is_from_local_daemon(&self.repo_name),
                        self.nostr.local_daemon_only(),
                    ) =>
                {
                    warn!(
                        "Fetch using local daemon root failed for {}: {}. Retrying via relays.",
                        self.repo_name, err
                    );
                    eprintln!("  Local daemon root failed; retrying via relays...");
                    let (refs, relay_root, _relay_key) = self
                        .nostr
                        .refetch_refs_without_local_daemon(&self.repo_name, 10)
                        .with_context(|| {
                            format!(
                                "refreshing {} from relays after local daemon fetch failure",
                                self.repo_name
                            )
                        })?;
                    self.remote_refs.clear();
                    for (name, sha) in refs {
                        self.remote_refs.insert(name, sha);
                    }
                    let relay_root = relay_root.ok_or_else(|| {
                        anyhow::anyhow!("relay refresh did not return a root hash")
                    })?;
                    self.fetch_git_objects_to_local_git(&relay_root)
                        .with_context(|| {
                            format!(
                                "fetching git objects after relay retry; local daemon error was: {}",
                                err
                            )
                        })?
                }
                Err(err) => return Err(err),
            };
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
                bail!("Failed to resolve .git/objects: {}", e);
            }
        };

        info!("Resolved .git/objects: {}", hex::encode(objects_cid.hash));
        let pack_locations = self
            .collect_git_pack_locations_async(&tree, &objects_cid)
            .await?;

        let fetch_tasks = self
            .collect_git_loose_object_locations_async(&tree, &objects_cid)
            .await?;

        Ok((tree, fetch_tasks, pack_locations, local_store_for_eviction))
    }

    async fn collect_git_loose_object_locations_async<S: Store>(
        &self,
        tree: &hashtree_core::HashTree<S>,
        objects_cid: &Cid,
    ) -> Result<Vec<GitObjectLocation>> {
        use futures::stream::{self, StreamExt};
        use hashtree_core::LinkType;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        let progress = StdArc::new(AtomicUsize::new(0));
        let done = StdArc::new(AtomicBool::new(false));

        eprintln!("  Loading objects tree...");
        let progress_clone = progress.clone();
        let done_clone = done.clone();
        let progress_task = tokio::spawn(async move {
            let mut last = 0;
            loop {
                tokio::time::sleep(fetch_progress_interval()).await;
                if done_clone.load(Ordering::Relaxed) {
                    break;
                }
                let current = progress_clone.load(Ordering::Relaxed);
                if current != last {
                    eprintln!("  Loading objects tree... {} entries", current);
                    last = current;
                }
            }
        });

        let objects_entries = match tree.list_directory(objects_cid).await {
            Ok(entries) => entries,
            Err(e) => {
                done.store(true, Ordering::Relaxed);
                let _ = progress_task.await;
                eprintln!("  Loading objects tree... failed: {}", e);
                warn!("Failed to list objects directory: {}", e);
                bail!("Failed to list objects directory: {}", e);
            }
        };
        debug!(
            ".git/objects entries while looking for loose objects: {:?}",
            objects_entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
        );
        progress.fetch_add(objects_entries.len(), Ordering::Relaxed);

        let mut fetch_tasks: Vec<GitObjectLocation> = Vec::new();
        let mut loose_prefixes = Vec::new();
        for entry in objects_entries {
            if is_git_hex_name(&entry.name, 2) {
                loose_prefixes.push((
                    entry.name,
                    Cid {
                        hash: entry.hash,
                        key: entry.key,
                    },
                ));
                continue;
            }

            if is_git_hex_name(&entry.name, 40) {
                fetch_tasks.push(GitObjectLocation {
                    oid: entry.name,
                    cid: Cid {
                        hash: entry.hash,
                        key: entry.key,
                    },
                });
            }
        }

        let tree_ref = tree;
        let prefix_results =
            stream::iter(loose_prefixes.into_iter().map(|(prefix, prefix_cid)| {
                let progress = progress.clone();
                async move {
                    let entries = tree_ref
                        .list_directory(&prefix_cid)
                        .await
                        .with_context(|| format!("list .git/objects/{prefix}"))?;
                    progress.fetch_add(entries.len(), Ordering::Relaxed);

                    let mut locations = Vec::new();
                    for entry in entries {
                        if entry.link_type == LinkType::Dir || !is_git_hex_name(&entry.name, 38) {
                            continue;
                        }
                        locations.push(GitObjectLocation {
                            oid: format!("{prefix}{}", entry.name),
                            cid: Cid {
                                hash: entry.hash,
                                key: entry.key,
                            },
                        });
                    }

                    Ok::<Vec<GitObjectLocation>, anyhow::Error>(locations)
                }
            }))
            .buffer_unordered(git_tree_walk_concurrency())
            .collect::<Vec<_>>()
            .await;

        done.store(true, Ordering::Relaxed);
        let _ = progress_task.await;

        for result in prefix_results {
            fetch_tasks.extend(result?);
        }
        fetch_tasks.sort_by(|left, right| left.oid.cmp(&right.oid));
        fetch_tasks.dedup_by(|left, right| left.oid == right.oid);

        eprintln!(
            "  Loading objects tree... done ({} loose objects)",
            fetch_tasks.len()
        );

        Ok(fetch_tasks)
    }

    async fn collect_git_pack_locations_async<S: Store>(
        &self,
        tree: &hashtree_core::HashTree<S>,
        objects_cid: &Cid,
    ) -> Result<Vec<GitPackLocation>> {
        let objects_entries = match tree.list_directory(objects_cid).await {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    "Failed to list .git/objects while looking for packs: {}",
                    err
                );
                return Ok(Vec::new());
            }
        };
        debug!(
            ".git/objects entries while looking for packs: {:?}",
            objects_entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
        );

        let Some(pack_dir_entry) = objects_entries.iter().find(|entry| entry.name == "pack") else {
            return Ok(Vec::new());
        };
        let pack_dir_cid = Cid {
            hash: pack_dir_entry.hash,
            key: pack_dir_entry.key,
        };

        let pack_entries: HashMap<_, _> = match tree.list_directory(&pack_dir_cid).await {
            Ok(entries) => entries
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect(),
            Err(err) => {
                warn!("Failed to list .git/objects/pack: {}", err);
                return Ok(Vec::new());
            }
        };
        debug!(
            ".git/objects/pack entries: {:?}",
            pack_entries.keys().cloned().collect::<Vec<_>>()
        );

        let mut info_packs_available = false;
        let mut pack_names = Vec::new();
        if let Some(info_dir_entry) = objects_entries.iter().find(|entry| entry.name == "info") {
            let info_dir_cid = Cid {
                hash: info_dir_entry.hash,
                key: info_dir_entry.key,
            };
            match tree.list_directory(&info_dir_cid).await {
                Ok(info_entries) => {
                    if let Some(info_packs_entry) =
                        info_entries.iter().find(|entry| entry.name == "packs")
                    {
                        let info_packs_cid = Cid {
                            hash: info_packs_entry.hash,
                            key: info_packs_entry.key,
                        };
                        match tree.get(&info_packs_cid, None).await {
                            Ok(Some(info_packs_bytes)) => {
                                info_packs_available = true;
                                let info_packs = String::from_utf8_lossy(&info_packs_bytes);
                                pack_names.extend(info_packs.lines().map(str::trim).filter_map(
                                    |line| {
                                        let pack_name = line.strip_prefix("P ")?;
                                        is_safe_git_pack_name(pack_name)
                                            .then(|| pack_name.to_string())
                                    },
                                ));
                            }
                            Ok(None) => {
                                warn!(
                                    ".git/objects/info/packs blob is missing; scanning .git/objects/pack"
                                );
                            }
                            Err(err) => {
                                warn!(
                                    "Failed to read .git/objects/info/packs; scanning .git/objects/pack: {}",
                                    err
                                );
                            }
                        }
                    } else {
                        warn!(".git/objects/info/packs not found; scanning .git/objects/pack");
                    }
                }
                Err(err) => {
                    warn!(
                        "Failed to list .git/objects/info; scanning .git/objects/pack: {}",
                        err
                    );
                }
            }
        } else {
            warn!(".git/objects/info not found; scanning .git/objects/pack");
        }

        if pack_names.is_empty() && !info_packs_available {
            pack_names.extend(
                pack_entries
                    .keys()
                    .filter(|name| is_safe_git_pack_name(name))
                    .cloned(),
            );
        }

        pack_names.sort();
        pack_names.dedup();

        let mut packs = Vec::new();
        for pack_name in pack_names {
            let Some(pack_entry) = pack_entries.get(&pack_name) else {
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
                pack_name,
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
        progress_bytes: Option<&std::sync::atomic::AtomicU64>,
    ) -> Result<u64> {
        use futures::StreamExt;
        use std::sync::atomic::Ordering;
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

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("stream {}", destination.display()))?;
            saw_chunk = true;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("write {}", temp_path.display()))?;
            written += chunk.len() as u64;
            if let Some(progress_bytes) = progress_bytes {
                progress_bytes.store(written, Ordering::Relaxed);
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
        Ok(written)
    }

    async fn install_git_pack_files_async<S: Store>(
        &self,
        tree: &hashtree_core::HashTree<S>,
        pack_locations: &[GitPackLocation],
    ) -> Result<usize> {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        if pack_locations.is_empty() {
            return Ok(0);
        }

        let git_pack_dir = Self::git_dir_path().join("objects").join("pack");
        std::fs::create_dir_all(&git_pack_dir)
            .with_context(|| format!("create {}", git_pack_dir.display()))?;
        let total_pack_size: u64 = pack_locations
            .iter()
            .map(|location| location.pack_size)
            .sum();
        let total_packs = pack_locations.len();

        let progress_done = StdArc::new(AtomicBool::new(false));
        let progress_notify = StdArc::new(tokio::sync::Notify::new());
        let processed_packs = StdArc::new(AtomicUsize::new(0));
        let current_pack = StdArc::new(AtomicUsize::new(0));
        let phase = StdArc::new(AtomicUsize::new(GIT_PACK_PHASE_IDLE));
        let loaded_pack_bytes = StdArc::new(AtomicU64::new(0));
        let current_pack_bytes = StdArc::new(AtomicU64::new(0));
        let stderr_is_terminal = std::io::stderr().is_terminal();
        let install_start = Instant::now();

        let initial_line = Self::format_git_pack_progress_line(
            0,
            total_packs,
            0,
            total_pack_size,
            0,
            GIT_PACK_PHASE_IDLE,
            false,
            Duration::ZERO,
        );
        Self::emit_git_pack_progress_line(&initial_line, stderr_is_terminal, false);

        let progress_task = {
            let progress_done = StdArc::clone(&progress_done);
            let progress_notify = StdArc::clone(&progress_notify);
            let processed_packs = StdArc::clone(&processed_packs);
            let current_pack = StdArc::clone(&current_pack);
            let phase = StdArc::clone(&phase);
            let loaded_pack_bytes = StdArc::clone(&loaded_pack_bytes);
            let current_pack_bytes = StdArc::clone(&current_pack_bytes);
            tokio::spawn(async move {
                let interval = git_pack_progress_interval(stderr_is_terminal);
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(interval) => {}
                        _ = progress_notify.notified() => {}
                    }
                    if progress_done.load(Ordering::Relaxed) {
                        break;
                    }
                    let line = RemoteHelper::format_git_pack_progress_line(
                        processed_packs.load(Ordering::Relaxed),
                        total_packs,
                        loaded_pack_bytes.load(Ordering::Relaxed)
                            + current_pack_bytes.load(Ordering::Relaxed),
                        total_pack_size,
                        current_pack.load(Ordering::Relaxed),
                        phase.load(Ordering::Relaxed),
                        false,
                        install_start.elapsed(),
                    );
                    RemoteHelper::emit_git_pack_progress_line(&line, stderr_is_terminal, false);
                }
            })
        };

        let install_result: Result<usize> = async {
            let mut installed = 0usize;
            for (index, location) in pack_locations.iter().enumerate() {
                current_pack.store(index + 1, Ordering::Relaxed);
                current_pack_bytes.store(0, Ordering::Relaxed);
                phase.store(GIT_PACK_PHASE_IDLE, Ordering::Relaxed);

                let pack_path = git_pack_dir.join(&location.pack_name);
                let idx_path = git_pack_dir.join(&location.idx_name);
                if pack_path.exists() && idx_path.exists() {
                    loaded_pack_bytes.fetch_add(location.pack_size, Ordering::Relaxed);
                    processed_packs.store(index + 1, Ordering::Relaxed);
                    continue;
                }

                let pack_size = if pack_path.exists() {
                    let pack_size = std::fs::metadata(&pack_path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(location.pack_size);
                    loaded_pack_bytes.fetch_add(pack_size, Ordering::Relaxed);
                    pack_size
                } else {
                    phase.store(GIT_PACK_PHASE_DOWNLOADING, Ordering::Relaxed);
                    let pack_size = Self::stream_git_pack_file(
                        tree,
                        &location.pack_cid,
                        &pack_path,
                        location.pack_name.clone(),
                        Some(location.pack_size),
                        Some(&current_pack_bytes),
                    )
                    .await
                    .with_context(|| format!("read {}", location.pack_name))?;
                    current_pack_bytes.store(0, Ordering::Relaxed);
                    loaded_pack_bytes.fetch_add(pack_size, Ordering::Relaxed);
                    pack_size
                };

                if let Some(idx_cid) = &location.idx_cid {
                    if !idx_path.exists() {
                        phase.store(GIT_PACK_PHASE_DOWNLOADING, Ordering::Relaxed);
                        Self::stream_git_pack_file(
                            tree,
                            idx_cid,
                            &idx_path,
                            location.idx_name.clone(),
                            location.idx_size,
                            None,
                        )
                        .await
                        .with_context(|| format!("read {}", location.idx_name))?;
                    }
                }

                if !idx_path.exists() {
                    phase.store(GIT_PACK_PHASE_INDEXING, Ordering::Relaxed);
                    let index_pack_path = pack_path.clone();
                    let status = tokio::task::spawn_blocking(move || {
                        Command::new("git")
                            .arg("index-pack")
                            .arg(&index_pack_path)
                            .status()
                    })
                    .await
                    .context("git index-pack task panicked")?
                    .context("run git index-pack")?;
                    if !status.success() {
                        bail!("git index-pack failed for {}", pack_path.display());
                    }
                }

                let _ = pack_size;
                processed_packs.store(index + 1, Ordering::Relaxed);
                phase.store(GIT_PACK_PHASE_IDLE, Ordering::Relaxed);
                installed += 1;
            }

            Ok(installed)
        }
        .await;

        progress_done.store(true, Ordering::Relaxed);
        progress_notify.notify_waiters();
        let _ = progress_task.await;

        match &install_result {
            Ok(_) => {
                let line = Self::format_git_pack_progress_line(
                    pack_locations.len(),
                    pack_locations.len(),
                    total_pack_size,
                    total_pack_size,
                    pack_locations.len(),
                    GIT_PACK_PHASE_IDLE,
                    true,
                    install_start.elapsed(),
                );
                Self::emit_git_pack_progress_line(&line, stderr_is_terminal, true);
            }
            Err(_) if stderr_is_terminal => {
                let line = format!(
                    "  Loading git packs: failed after {:.1}s",
                    install_start.elapsed().as_secs_f32()
                );
                Self::emit_git_pack_progress_line(&line, true, true);
            }
            Err(_) => {}
        }

        install_result
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
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        let total_objects = fetch_tasks.len();

        let downloaded = StdArc::new(AtomicUsize::new(0));
        let download_done = StdArc::new(AtomicBool::new(false));

        // Spawn progress reporter
        let downloaded_clone = downloaded.clone();
        let download_done_clone = download_done.clone();
        let total_for_timer = total_objects;
        eprintln!("  Loading {} loose git object(s)", total_objects);
        let timer_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(fetch_progress_interval()).await;
                if download_done_clone.load(Ordering::Relaxed) {
                    break;
                }
                let count = downloaded_clone.load(Ordering::Relaxed);
                eprintln!("  Loading loose git objects: {}/{}", count, total_for_timer);
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
        eprintln!(
            "  Loading loose git objects: {}/{}",
            success_count, total_objects
        );

        // Retry failed downloads sequentially
        let mut missing_objects: Vec<(String, String)> = Vec::new(); // (oid, hash)
        if !failed.is_empty() {
            eprintln!("  Retrying {} failed downloads...", failed.len());
            for (i, (oid, obj_cid)) in failed.iter().enumerate() {
                let hash_hex = hex::encode(obj_cid.hash);
                eprintln!("  Retrying {}/{}: {}...", i + 1, failed.len(), oid);

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
            self.install_git_pack_files_async(&tree, &pack_locations)
                .await
                .context("install advertised git pack files")?;
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
        let progress_interval = fetch_progress_interval();
        let mut last_progress = Instant::now();

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

        eprintln!("  Fetching {} loose git object(s)", total_to_write);
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

            if completed < total_to_write && last_progress.elapsed() >= progress_interval {
                eprintln!(
                    "  Fetching loose git objects: {}/{}",
                    completed, total_to_write
                );
                last_progress = Instant::now();
            }
        }

        let mut missing_objects: Vec<(String, String)> = Vec::new();
        if !failed.is_empty() {
            eprintln!("\n  Retrying {} failed downloads...", failed.len());
            for (i, (oid, obj_cid)) in failed.iter().enumerate() {
                let hash_hex = hex::encode(obj_cid.hash);
                eprintln!("  Retrying {}/{}: {}...", i + 1, failed.len(), oid);

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
                "  Retried: {}/{} objects available",
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
            eprintln!("  Writing to .git: {} new, {} cached", written, cached);
        } else {
            eprintln!("  Writing to .git: {}/{}", written, written);
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
