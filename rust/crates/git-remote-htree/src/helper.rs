//! Git remote helper protocol implementation
//!
//! Implements the stateless git remote helper protocol.
//! See: https://git-scm.com/docs/gitremote-helpers

use crate::git::storage::GitStorage;
use crate::runtime::block_on_result;
use anyhow::{bail, Context, Result};
use hashtree_core::{Cid, Store};
use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};
#[cfg(test)]
use std::{collections::HashSet, process::Command};
use tracing::{debug, info, warn};

mod cached_store;
mod git_objects;
mod progress;
mod push;
mod storage_support;

use progress::UploadProgress;
#[cfg(test)]
use storage_support::{build_repo_viewer_url, queue_hash_if_new};
use storage_support::{create_cached_local_store, get_hashtree_data_dir};

/// Threshold for showing detailed progress (3 seconds)
const VERBOSE_THRESHOLD: Duration = Duration::from_secs(3);
/// Number of old-tree hashes to probe per server before deciding whether an
/// incremental push can safely skip unchanged content.
const SERVER_COVERAGE_SAMPLE_SIZE: usize = 32;

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

impl RemoteHelper {
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
        // For push, always return empty refs to force re-push
        // This ensures content is always re-uploaded to blossom servers
        // and we regenerate the index file each time
        if for_push {
            debug!("Returning empty refs for push to force re-upload");
            self.remote_refs.clear();
            return Ok(Some(vec![String::new()]));
        }

        // For clone/pull, fetch actual refs from nostr
        self.remote_refs.clear();
        let refs = self.nostr.fetch_refs(&self.repo_name)?;

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

        let data_dir = get_hashtree_data_dir();
        let blobs_path = data_dir.join("blobs");
        let (local_store, _is_shared_cache) = create_cached_local_store(&blobs_path);
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
                return Ok((tree, Vec::new(), local_store_for_eviction));
            }
            Err(e) => {
                warn!("Failed to resolve .git/objects: {}", e);
                return Ok((tree, Vec::new(), local_store_for_eviction));
            }
        };

        info!("Resolved .git/objects: {}", hex::encode(objects_cid.hash));

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

        const WALK_CONCURRENCY: usize = 32;
        let walk_entries = match tree
            .walk_parallel_with_progress(&objects_cid, "", WALK_CONCURRENCY, Some(&progress))
            .await
        {
            Ok(entries) => entries,
            Err(e) => {
                done.store(true, Ordering::Relaxed);
                let _ = progress_task.await;
                eprintln!("\r  Loading objects tree... failed: {}", e);
                warn!("Failed to walk objects directory: {}", e);
                return Ok((tree, Vec::new(), local_store_for_eviction));
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

        Ok((tree, fetch_tasks, local_store_for_eviction))
    }

    /// Async implementation of git object fetching using HashTree helpers
    async fn fetch_git_objects_async(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut objects = Vec::new();
        let (tree, fetch_tasks, local_store_for_eviction) = self
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
        const CONCURRENCY: usize = 20;
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
            .buffer_unordered(CONCURRENCY)
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
        let (tree, fetch_tasks, local_store_for_eviction) = self
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

        const CONCURRENCY: usize = 20;
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
        .buffer_unordered(CONCURRENCY);

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
