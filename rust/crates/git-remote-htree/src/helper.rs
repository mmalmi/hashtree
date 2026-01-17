//! Git remote helper protocol implementation
//!
//! Implements the stateless git remote helper protocol.
//! See: https://git-scm.com/docs/gitremote-helpers

use anyhow::{bail, Context, Result};
use crate::git::object::ObjectType;
use crate::git::refs::Ref;
use crate::git::storage::GitStorage;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Threshold for showing detailed progress (3 seconds)
const VERBOSE_THRESHOLD: Duration = Duration::from_secs(3);

use hashtree_config::Config;
use crate::nostr_client::{BlossomResult, NostrClient, RelayResult};

// CachedStore: local store first, then Blossom fallback
mod cached_store {
    use hashtree_blossom::BlossomStore;
    use hashtree_core::{Hash, Store, StoreError};
    use std::sync::Arc;

    pub struct CachedStore {
        local: Arc<dyn Store + Send + Sync>,
        blossom: BlossomStore,
    }

    impl CachedStore {
        pub fn new(local: Arc<dyn Store + Send + Sync>, blossom: BlossomStore) -> Self {
            Self { local, blossom }
        }
    }

    #[async_trait::async_trait]
    impl Store for CachedStore {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            // Store locally
            self.local.put(hash, data).await
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            // Try local first
            if let Ok(Some(data)) = self.local.get(hash).await {
                return Ok(Some(data));
            }
            // Fallback to Blossom
            let result = self.blossom.get(hash).await;
            // Cache locally if found
            if let Ok(Some(ref data)) = result {
                let _ = self.local.put(*hash, data.clone()).await;
            }
            result
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            // Check local first
            if self.local.has(hash).await? {
                return Ok(true);
            }
            // Fallback to Blossom
            self.blossom.has(hash).await
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            // Delete from local only (don't delete from remote)
            self.local.delete(hash).await
        }
    }
}

/// Get the shared hashtree data directory
fn get_hashtree_data_dir() -> PathBuf {
    hashtree_config::get_data_dir()
}

/// Create local blob store based on config
fn create_local_store(path: &std::path::Path) -> Result<std::sync::Arc<dyn hashtree_core::Store + Send + Sync>> {
    use hashtree_config::StorageBackend;
    use hashtree_fs::FsBlobStore;

    let config = Config::load_or_default();
    match config.storage.backend {
        StorageBackend::Fs => {
            Ok(std::sync::Arc::new(FsBlobStore::new(path)?))
        }
        #[cfg(feature = "lmdb")]
        StorageBackend::Lmdb => {
            Ok(std::sync::Arc::new(hashtree_lmdb::LmdbBlobStore::new(path)?))
        }
        #[cfg(not(feature = "lmdb"))]
        StorageBackend::Lmdb => {
            warn!("LMDB backend requested but lmdb feature not enabled, using filesystem storage");
            Ok(std::sync::Arc::new(FsBlobStore::new(path)?))
        }
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
        let refs = self.nostr.fetch_refs(&self.repo_name)?;

        let mut lines = Vec::new();
        self.remote_refs.clear();

        for (name, sha) in &refs {
            self.remote_refs.insert(name.clone(), sha.clone());
            if name == "HEAD" {
                // HEAD is a symref - check for actual target branch
                if let Some(target_branch) = sha.strip_prefix("ref: ") {
                    // Symbolic ref (e.g., "ref: refs/heads/main")
                    if let Some(target_sha) = refs.get(target_branch) {
                        lines.push(format!("@{} HEAD", target_branch));
                        lines.push(format!("{} HEAD", target_sha));
                    }
                } else if let Some((branch, target)) = refs
                    .get("refs/heads/main")
                    .map(|t| ("refs/heads/main", t))
                    .or_else(|| refs.get("refs/heads/master").map(|t| ("refs/heads/master", t)))
                {
                    // Direct SHA in HEAD, find the matching branch
                    lines.push(format!("@{} HEAD", branch));
                    lines.push(format!("{} HEAD", target));
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

        // Get the cached root hash from nostr (set during list command)
        let root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();

        if let Some(ref root) = root_hash {
            // Fetch all git objects from the hashtree structure
            let objects = self.fetch_all_git_objects(root)?;
            info!("Loaded {} git objects from hashtree", objects.len());

            // Batch check which objects git already has
            let existing = self.git_batch_check_objects(objects.iter().map(|(oid, _)| oid.as_str()))?;

            // Filter to only objects git doesn't have
            let to_write: Vec<_> = objects.into_iter()
                .filter(|(oid, _)| !existing.contains(oid))
                .collect();

            let total = to_write.len();
            let skipped = existing.len();

            if total == 0 {
                eprintln!("  Writing to .git: 0 new, {} cached    ", skipped);
            } else {
                for (i, (oid, data)) in to_write.into_iter().enumerate() {
                    self.write_git_object(&oid, &data)?;
                    let count = i + 1;
                    if count % 50 == 0 || count == total || count == 1 {
                        eprint!("\r  Writing to .git: {}/{}    ", count, total);
                        let _ = std::io::stderr().flush();
                    }
                }
                if skipped > 0 {
                    eprintln!("\r  Writing to .git: {} new, {} cached    ", total, skipped);
                } else {
                    eprintln!("\r  Writing to .git: {}/{}    ", total, total);
                }
            }
        } else {
            bail!("No root hash found for repository - cannot fetch");
        }

        // Update local refs to point to the fetched commits
        // Use git update-ref since git sets GIT_DIR for the remote helper
        for spec in &self.fetch_specs {
            let output = Command::new("git")
                .args(["update-ref", &spec.name, &spec.sha])
                .output()
                .context("Failed to run git update-ref")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("Failed to update ref {}: {}", spec.name, stderr);
            } else {
                debug!("Updated {} -> {}", spec.name, &spec.sha[..12]);
            }
        }

        self.fetch_specs.clear();
        Ok(())
    }

    /// Fetch all git objects from hashtree's .git/objects/ directory
    fn fetch_all_git_objects(&self, root_hash: &str) -> Result<Vec<(String, Vec<u8>)>> {
        // NostrClient now handles unmasking for link-visible repos (url_secret)
        // The cached key is already the real CHK key
        let encryption_key = self.nostr.get_cached_encryption_key(&self.repo_name).cloned();

        info!("fetch_all_git_objects: root={}, has encryption_key: {}, link_visible: {}",
              &root_hash[..12], encryption_key.is_some(), self.url_secret.is_some());

        // Create tokio runtime for async blossom downloads
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

        rt.block_on(self.fetch_git_objects_async(root_hash, encryption_key.as_ref()))
    }

    /// Async implementation of git object fetching using HashTree helpers
    async fn fetch_git_objects_async(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        use hashtree_blossom::BlossomStore;
        use hashtree_core::{Cid, HashTree, HashTreeConfig};

        let blossom = self.nostr.blossom();
        let mut objects = Vec::new();

        // Log the servers being used
        let servers = blossom.read_servers().to_vec();
        info!("Creating CachedStore with local + Blossom (servers: {:?})", servers);

        // Create local blob store based on config
        let data_dir = get_hashtree_data_dir();
        let blobs_path = data_dir.join("blobs");
        let local_store = create_local_store(&blobs_path)
            .context("Failed to create local blob store")?;

        // Create Blossom store for remote fallback
        let blossom_store = BlossomStore::with_servers(
            nostr::Keys::generate(), // Temporary keys for read-only ops
            servers,
        );

        // Create cached store: local first, then Blossom
        let store = cached_store::CachedStore::new(local_store, blossom_store);
        let tree = HashTree::new(HashTreeConfig::new(std::sync::Arc::new(store)));

        // Parse root hash and create Cid with encryption key
        let root_bytes = hex::decode(root_hash)
            .context("Invalid root hash hex")?;
        let root_arr: [u8; 32] = root_bytes.try_into()
            .map_err(|_| anyhow::anyhow!("Root hash must be 32 bytes"))?;

        let root_cid = Cid {
            hash: root_arr,
            key: encryption_key.copied(),
        };

        // Resolve .git/objects path
        let objects_cid = match tree.resolve_path(&root_cid, ".git/objects").await {
            Ok(Some(cid)) => cid,
            Ok(None) => {
                warn!("No .git/objects directory found");
                return Ok(objects);
            }
            Err(e) => {
                warn!("Failed to resolve .git/objects: {}", e);
                return Ok(objects);
            }
        };

        info!("Resolved .git/objects: {}", hex::encode(objects_cid.hash));

        use futures::stream::{self, StreamExt};
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
        use std::sync::Arc as StdArc;
        use hashtree_core::LinkType;

        // Walk the objects tree with parallel fetching and progress reporting
        let progress = StdArc::new(AtomicUsize::new(0));
        let done = StdArc::new(AtomicBool::new(false));

        // Spawn progress reporter
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
        let walk_entries = match tree.walk_parallel_with_progress(&objects_cid, "", WALK_CONCURRENCY, Some(&progress)).await {
            Ok(entries) => entries,
            Err(e) => {
                done.store(true, Ordering::Relaxed);
                let _ = progress_task.await;
                eprintln!("\r  Loading objects tree... failed: {}", e);
                warn!("Failed to walk objects directory: {}", e);
                return Ok(objects);
            }
        };
        done.store(true, Ordering::Relaxed);
        let _ = progress_task.await;
        let walk_done_time = std::time::Instant::now();
        if self.is_slow() {
            eprintln!("\r  Loading objects tree... done ({} entries)        ", walk_entries.len());
        } else {
            eprint!("\r                                                        \r"); // Clear the line
        }

        // Extract git objects from walk entries (files with 40 char hex names like "ab/cdef..." -> "abcdef...")
        let mut fetch_tasks: Vec<(String, Cid)> = Vec::new();
        for entry in walk_entries {
            // Skip directories
            if entry.link_type == LinkType::Dir {
                continue;
            }

            // Parse path like "ab/cdef1234..." into oid "abcdef1234..."
            let parts: Vec<&str> = entry.path.split('/').collect();
            if parts.len() == 2 && parts[0].len() == 2 && parts[1].len() == 38 {
                if hex::decode(parts[0]).is_ok() && hex::decode(parts[1]).is_ok() {
                    let oid = format!("{}{}", parts[0], parts[1]);
                    let obj_cid = Cid {
                        hash: entry.hash,
                        key: entry.key,
                    };
                    fetch_tasks.push((oid, obj_cid));
                }
            } else if parts.len() == 1 && parts[0].len() == 40 {
                // Flat layout: object files directly in objects/
                if hex::decode(parts[0]).is_ok() {
                    let oid = parts[0].to_string();
                    let obj_cid = Cid {
                        hash: entry.hash,
                        key: entry.key,
                    };
                    fetch_tasks.push((oid, obj_cid));
                }
            }
        }

        let total_objects = fetch_tasks.len();
        let prep_elapsed = walk_done_time.elapsed();
        if self.is_slow() {
            eprintln!("  Prepared {} objects in {:?}", total_objects, prep_elapsed);
        }

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

        // First pass: fetch all objects with normal timeout
        let results: Vec<Result<(String, Vec<u8>), (String, Cid)>> = stream::iter(fetch_tasks)
            .map(|(oid, obj_cid)| {
                let tree = &tree;
                let downloaded = StdArc::clone(&downloaded);
                async move {
                    let result = match tree.get(&obj_cid).await {
                        Ok(Some(content)) => Ok((oid, content)),
                        Ok(None) => Err((oid, obj_cid)),
                        Err(_) => Err((oid, obj_cid)),
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

                match tree.get(obj_cid).await {
                    Ok(Some(content)) => {
                        objects.push((oid.clone(), content));
                    }
                    Ok(None) => {
                        eprintln!("\n  ERROR: Object {} not found (hash: {})", oid, hash_hex);
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                    Err(e) => {
                        eprintln!("\n  ERROR: Failed to fetch {}: {} (hash: {})", oid, e, hash_hex);
                        missing_objects.push((oid.clone(), hash_hex));
                    }
                }
            }
            eprintln!("\r  Retried: {}/{} objects available        ", objects.len(), total_objects);
        }

        // Fail if any objects are missing - git clone will fail anyway
        if !missing_objects.is_empty() {
            let obj_list: Vec<String> = missing_objects.iter()
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
        Ok(objects)
    }

    /// Batch check which objects git already has (returns set of existing oids)
    fn git_batch_check_objects<'a>(&self, oids: impl Iterator<Item = &'a str>) -> Result<HashSet<String>> {
        let mut existing = HashSet::new();
        let oids: Vec<_> = oids.collect();

        // Process in chunks to avoid memory issues with huge repos
        const BATCH_SIZE: usize = 1000;
        for chunk in oids.chunks(BATCH_SIZE) {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch-check=%(objectname)"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to spawn git cat-file")?;

            {
                let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
                for oid in chunk {
                    writeln!(stdin, "{}", oid)?;
                }
            }

            let output = child.wait_with_output().context("Failed to read git cat-file output")?;

            // Parse output - valid objects return just the oid, missing ones return "oid missing"
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let line = line.trim();
                if line.len() == 40 && !line.contains(' ') {
                    existing.insert(line.to_string());
                }
            }
        }
        Ok(existing)
    }

    /// Write loose object to local git object store
    /// The data is zlib-compressed loose object format - decompress and use git hash-object
    fn write_git_object(&self, oid: &str, data: &[u8]) -> Result<()> {
        use flate2::read::ZlibDecoder;
        use std::io::Read;

        // Git objects are stored as .git/objects/xx/yy... where xx is first 2 chars
        if oid.len() < 3 {
            bail!("Invalid object id: {}", oid);
        }

        // Decompress the zlib data to get the raw git object (header + content)
        let mut decoder = ZlibDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)
            .context("Failed to decompress git object")?;

        // Parse git object format: "<type> <size>\0<content>"
        let null_pos = decompressed.iter().position(|&b| b == 0)
            .context("Invalid git object: no null byte")?;

        let header = std::str::from_utf8(&decompressed[..null_pos])
            .context("Invalid git object header")?;

        let content = &decompressed[null_pos + 1..];

        // Parse header to get type
        let parts: Vec<&str> = header.split(' ').collect();
        if parts.len() != 2 {
            bail!("Invalid git object header: {}", header);
        }
        let obj_type = parts[0];

        // Use git hash-object to write the object - this works during clone
        // because git sets GIT_DIR for the remote helper
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "-t", obj_type, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn git hash-object")?;

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(content)?;
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git hash-object failed: {}", stderr);
        }

        let written_oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if written_oid != oid {
            warn!("Object hash mismatch: expected {}, got {}", oid, written_oid);
        }

        debug!("Wrote git object {} via hash-object", oid);
        Ok(())
    }

    /// Queue a push operation
    fn queue_push(&mut self, arg: &str) -> Result<()> {
        // Format: [+]<src>:<dst>
        let force = arg.starts_with('+');
        let arg = if force { &arg[1..] } else { arg };

        let parts: Vec<&str> = arg.splitn(2, ':').collect();
        if parts.len() != 2 {
            bail!("Invalid push spec: {}", arg);
        }

        self.push_specs.push(PushSpec {
            src: parts[0].to_string(),
            dst: parts[1].to_string(),
            force,
        });
        Ok(())
    }

    /// Execute queued push operations
    fn execute_push(&mut self) -> Result<Option<Vec<String>>> {
        self.start_op(); // Start timing for conditional verbose logging
        debug!(refs_count = self.push_specs.len(), "execute_push called");
        info!("Pushing {} refs", self.push_specs.len());

        // First, load existing refs and objects from remote to preserve other branches
        // Check if any push is a force push
        let has_force_push = self.push_specs.iter().any(|s| s.force);
        debug!(force = has_force_push, "About to call load_existing_remote_state");

        if let Err(e) = self.load_existing_remote_state() {
            let err_str = e.to_string();

            // Check if this is an access restriction error (changing visibility modes)
            // These are expected and we should proceed with fresh state
            let is_access_error = err_str.contains("link-visible")
                || err_str.contains("private")
                || err_str.contains("secret key");

            // Check if this might be a new repo (no refs found is OK)
            let is_likely_new_repo = err_str.contains("No root hash")
                || err_str.contains("not found")
                || err_str.contains("timeout");

            if is_access_error {
                // Changing visibility mode - proceed with fresh state
                debug!("Cannot access existing repo (visibility change): {}", e);
            } else if has_force_push {
                // Force push - proceed without existing state
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Proceeding with force push (may overwrite other branches)");
            } else if is_likely_new_repo {
                debug!("Error loading remote state (likely new repo): {}", e);
                info!("Could not load existing remote state: {} (likely new repo)", e);
            } else {
                // There's an existing remote but we can't load it - warn user
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Other branches may be lost. Use 'git push --force' to override.");
                eprintln!("  Or check your network connection and try again.");
            }
        }

        let mut results = Vec::new();

        // Clone specs to avoid borrow issues
        let specs: Vec<_> = std::mem::take(&mut self.push_specs);

        for spec in specs {
            debug!(
                "Pushing {} -> {} (force={})",
                spec.src, spec.dst, spec.force
            );

            // Resolve src to sha
            let sha = if spec.src.is_empty() {
                // Delete ref
                String::new()
            } else {
                self.resolve_ref(&spec.src)?
            };

            if sha.is_empty() {
                // Delete
                match self.storage.delete_ref(&spec.dst) {
                    Ok(_) => {
                        self.nostr.delete_ref(&self.repo_name, &spec.dst)?;
                        results.push(format!("ok {}", spec.dst));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            } else {
                // Check for non-fast-forward push (unless force)
                if !spec.force {
                    if let Some(remote_sha) = self.remote_refs.get(&spec.dst) {
                        if !self.is_ancestor(remote_sha, &sha) {
                            results.push(format!(
                                "error {} non-fast-forward (use --force to override)",
                                spec.dst
                            ));
                            eprintln!(
                                "  Rejected: {} has commits you don't have. Pull first or use --force.",
                                spec.dst
                            );
                            continue;
                        }
                    }
                }

                // Push objects
                match self.push_objects(&sha, &spec.dst) {
                    Ok(()) => results.push(format!("ok {}", spec.dst)),
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            }
        }

        results.push(String::new()); // Empty line terminates
        Ok(Some(results))
    }

    /// Load existing refs and objects from remote before pushing
    /// This preserves branches that aren't being pushed
    fn load_existing_remote_state(&mut self) -> Result<()> {
        let data_dir = get_hashtree_data_dir();
        self.detail(&format!("  Loading existing remote state... (data_dir: {:?})", data_dir));

        // Fetch refs from nostr (this also caches root hash)
        let (refs, root_hash, _encryption_key) = self.nostr.fetch_refs_with_root(&self.repo_name)?;

        if refs.is_empty() {
            self.detail("  No existing refs found (new repository)");
            return Ok(());
        }

        self.detail(&format!("  Found {} existing refs", refs.len()));

        // Store remote refs for non-fast-forward detection
        self.remote_refs.clear();
        for (ref_name, ref_value) in &refs {
            // Only track branch refs (not HEAD symref)
            if ref_name.starts_with("refs/") && !ref_value.starts_with("ref: ") {
                self.remote_refs.insert(ref_name.clone(), ref_value.clone());
            }
        }

        // Import refs into storage (these will be merged with pushed refs)
        for (ref_name, ref_value) in &refs {
            // Skip refs that we're about to push (they'll be overwritten anyway)
            let is_being_pushed = self.push_specs.iter().any(|s| s.dst == *ref_name);
            if !is_being_pushed {
                self.storage.import_ref(ref_name, ref_value)?;
                debug!("Imported existing ref: {} -> {}", ref_name, &ref_value[..12.min(ref_value.len())]);
            }
        }

        // Fetch all git objects from remote hashtree
        if let Some(root) = root_hash {
            let objects = self.fetch_all_git_objects(&root)?;
            self.detail(&format!("  Importing {} existing objects", objects.len()));

            for (oid, content) in objects {
                // Content from hashtree is already the compressed loose object
                // (that's what we store in build_objects_dir)
                self.storage.import_compressed_object(&oid, content)?;
            }
        }

        self.detail("  Remote state loaded");
        Ok(())
    }

    /// Resolve a ref to its sha
    fn resolve_ref(&self, refspec: &str) -> Result<String> {
        let output = Command::new("git").args(["rev-parse", refspec]).output()?;

        if !output.status.success() {
            bail!("Failed to resolve ref: {}", refspec);
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check if ancestor_sha is an ancestor of descendant_sha
    fn is_ancestor(&self, ancestor_sha: &str, descendant_sha: &str) -> bool {
        // git merge-base --is-ancestor returns 0 if true, 1 if false
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor_sha, descendant_sha])
            .output();

        match output {
            Ok(o) => o.status.success(),
            Err(_) => false, // If we can't check, assume not an ancestor (safer)
        }
    }

    /// Push all objects reachable from sha
    fn push_objects(&mut self, sha: &str, dst_ref: &str) -> Result<()> {
        // Get list of objects to push
        eprint!("  Listing objects...");
        let _ = std::io::stderr().flush();
        let objects = self.list_objects_to_push(sha)?;
        eprintln!(" {} objects", objects.len());

        info!("Pushing {} objects for {}", objects.len(), sha);

        // Read all objects in batch using git cat-file --batch
        let objects_with_content = self.read_git_objects_batch(&objects)?;
        eprintln!(); // Newline after reading progress

        eprint!("  Writing to local store...");
        let _ = std::io::stderr().flush();
        let total = objects_with_content.len();
        for (i, (obj_type, content)) in objects_with_content.into_iter().enumerate() {
            self.storage.write_raw_object(obj_type, &content)?;
            if (i + 1) % 1000 == 0 || i + 1 == total {
                eprint!("\r  Writing to local store: {}/{}", i + 1, total);
                let _ = std::io::stderr().flush();
            }
        }
        eprintln!();

        // Update ref in storage
        let oid = crate::git::object::ObjectId::from_hex(sha)
            .ok_or_else(|| anyhow::anyhow!("Invalid object id: {}", sha))?;
        self.storage.write_ref(dst_ref, &Ref::Direct(oid))?;

        // Set HEAD to point to this branch if it's a branch ref
        // This is needed for wasm-git to detect the current branch
        if dst_ref.starts_with("refs/heads/") {
            self.storage.write_ref("HEAD", &Ref::Symbolic(dst_ref.to_string()))?;
            debug!("Set HEAD -> {}", dst_ref);
        }

        // Check if we can sign before doing any work
        if !self.nostr.can_sign() {
            anyhow::bail!(
                "Cannot push: no secret key for {}. You can only push to your own repos.",
                self.nostr.npub()
            );
        }

        // Build the merkle tree
        if self.is_slow() {
            eprint!("  Building merkle tree...");
            let _ = std::io::stderr().flush();
        }
        let root_cid = self.storage.build_tree()?;
        let root_hash_hex = hex::encode(root_cid.hash);
        let chk_key = root_cid.key;
        let is_link_visible = self.url_secret.is_some();
        if self.is_slow() {
            eprintln!(" done (encrypted: {}, link_visible: {}, private: {})", chk_key.is_some(), is_link_visible, self.is_private);
        }

        // For private repos: XOR the CHK key with url_secret so only URL holders can decrypt
        // For public repos: publish the CHK key directly
        let key_to_publish = if let (Some(chk), Some(secret)) = (chk_key, self.url_secret) {
            // XOR the keys - to decrypt, recipient XORs with their copy of secret
            let mut masked = [0u8; 32];
            for i in 0..32 {
                masked[i] = chk[i] ^ secret[i];
            }
            Some(masked)
        } else {
            chk_key
        };

        // Push to file servers (blossom) first
        // This makes content available before we advertise the hash
        // Get old root hash if it exists (for efficient diff-based upload)
        let old_root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();
        let old_encryption_key = self.nostr.get_cached_encryption_key(&self.repo_name).copied();
        let blossom_result = self.push_to_file_servers_with_diff(
            &root_hash_hex,
            chk_key.as_ref(),
            old_root_hash.as_deref(),
            old_encryption_key.as_ref(),
        );

        // Then publish to nostr (kind 30078 with hashtree label)
        // Include masked key (encryptedKey tag) for private or raw CHK key (key tag) for public repos
        // Don't fail push if relay publish fails - it's just distribution
        let key_with_privacy = key_to_publish.as_ref().map(|k| (k, is_link_visible, self.is_private));
        let (npub_url, relay_result) = match self.nostr.publish_repo(&self.repo_name, &root_hash_hex, key_with_privacy) {
            Ok((url, result)) => (url, result),
            Err(e) => {
                warn!("Failed to publish to relays: {}", e);
                // Construct URL anyway for display using npub
                let url = format!("htree://{}/{}", self.nostr.npub(), &self.repo_name);
                let configured = self.nostr.relay_urls();
                (url, RelayResult { configured: configured.clone(), connected: vec![], failed: configured })
            }
        };

        // Build full URL with secret fragment if private
        let full_url = if let Some(secret) = self.url_secret {
            format!("{}#k={}", npub_url, hex::encode(secret))
        } else {
            npub_url.clone()
        };

        // Print summary
        eprintln!("Published to: {}", full_url);

        // Print relay details
        if !relay_result.connected.is_empty() {
            eprintln!("  Relays: {}", relay_result.connected.join(", "));
        } else {
            eprintln!("  Relays: none");
        }
        if !relay_result.failed.is_empty() {
            eprintln!("  Relays failed: {}", relay_result.failed.join(", "));
        }

        // Print blossom details
        if !blossom_result.succeeded.is_empty() {
            eprintln!("  Blossom: {}", blossom_result.succeeded.join(", "));
        }
        if !blossom_result.failed.is_empty() {
            eprintln!("  Blossom failed: {}", blossom_result.failed.join(", "));
        }

        eprintln!("  Config: ~/.hashtree/config.toml");

        // Print web viewer URL
        if let Some(path) = npub_url.strip_prefix("htree://") {
            let viewer_url = if let Some(secret) = self.url_secret {
                format!("https://files.iris.to/#/{}?k={}", path, hex::encode(secret))
            } else {
                format!("https://files.iris.to/#/{}", path)
            };
            eprintln!("View at: {}", viewer_url);
        }

        Ok(())
    }

    /// Push content to file servers (blossom) with efficient diff-based upload
    ///
    /// When an old root hash is provided, computes the diff and only uploads
    /// hashes that don't exist in the old tree. This significantly reduces
    /// upload time for incremental pushes.
    ///
    /// Returns BlossomResult with server details
    fn push_to_file_servers_with_diff(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
        old_root_hash: Option<&str>,
        old_encryption_key: Option<&[u8; 32]>,
    ) -> BlossomResult {
        use hashtree_core::try_decode_tree_node;
        use hashtree_core::crypto::decrypt_chk;

        let store = self.storage.store();
        let blossom = self.nostr.blossom();
        let configured: Vec<String> = blossom.write_servers().to_vec();

        // Create runtime for async uploads
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!("Failed to create runtime for blossom upload: {}", e);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                };
            }
        };

        // Parse root hash
        let root_bytes = match hex::decode(root_hash) {
            Ok(b) if b.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&b);
                arr
            }
            _ => {
                warn!("Invalid root hash: {}", root_hash);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                };
            }
        };

        // Parse old root hash if provided
        let old_root_bytes: Option<[u8; 32]> = old_root_hash.and_then(|h| {
            hex::decode(h).ok().and_then(|b| {
                if b.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    Some(arr)
                } else {
                    None
                }
            })
        });

        let verbose = self.is_slow(); // Capture before async block
        let force_upload = self.config.blossom.force_upload;
        let success = rt.block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::sync::Arc;
            use tokio::sync::mpsc;
            use hashtree_core::{HashTree, HashTreeConfig, Cid, collect_hashes};

            let uploaded = Arc::new(AtomicUsize::new(0));
            let skipped_diff = Arc::new(AtomicUsize::new(0)); // Skipped due to diff (already in old tree)
            let skipped_server = Arc::new(AtomicUsize::new(0)); // Skipped due to server already having it
            let failed = Arc::new(AtomicUsize::new(0));
            let processed = Arc::new(AtomicUsize::new(0));

            // Collect old tree hashes if we have an old root
            let old_hashes: HashSet<[u8; 32]> = if let Some(old_root) = old_root_bytes {
                // Check if old and new root are the same (no changes)
                if old_root == root_bytes {
                    if verbose {
                        eprintln!("  No changes detected (same root hash)");
                    }
                    return true;
                }

                if verbose {
                    eprint!("  Computing diff from previous tree...");
                    let _ = std::io::stderr().flush();
                }

                // Create a HashTree for the store to use collect_hashes
                let cached_store = cached_store::CachedStore::new(
                    store.clone(),
                    hashtree_blossom::BlossomStore::new(blossom.clone()),
                );
                let tree = HashTree::new(HashTreeConfig::new(Arc::new(cached_store)));
                let old_cid = Cid {
                    hash: old_root,
                    key: old_encryption_key.copied(),
                };

                match collect_hashes(&tree, &old_cid, 32).await {
                    Ok(hashes) => {
                        if verbose {
                            eprintln!(" {} hashes in old tree", hashes.len());
                        }
                        hashes
                    }
                    Err(e) => {
                        if verbose {
                            eprintln!(" failed: {}", e);
                            eprintln!("  Falling back to full upload");
                        }
                        HashSet::new()
                    }
                }
            } else {
                HashSet::new()
            };

            let has_old_tree = !old_hashes.is_empty();

            // Check which servers need full upload (don't have old tree)
            // If force_upload is true, all servers get full upload (skip server-has check)
            let all_servers: Vec<String> = blossom.write_servers().to_vec();
            let servers_needing_full: Arc<Vec<String>> = if force_upload {
                // Force upload to all servers
                Arc::new(all_servers.clone())
            } else if has_old_tree && !all_servers.is_empty() {
                // Always include the root hash first, then sample additional random hashes
                // Root is critical - if server doesn't have root, it can't serve the tree
                let old_root = old_root_bytes.unwrap();
                let mut sample_hashes = vec![hex::encode(old_root)];
                // Add up to 4 more random samples from the rest of the tree
                for hash in old_hashes.iter().take(4) {
                    if *hash != old_root {
                        sample_hashes.push(hex::encode(hash));
                    }
                }
                let sample_refs: Vec<&str> = sample_hashes.iter().map(|s| s.as_str()).collect();
                let mut needs_full = Vec::new();
                for server in &all_servers {
                    if !blossom.server_has_tree_samples(server, &sample_refs, 5).await {
                        needs_full.push(server.clone());
                    }
                }
                if !needs_full.is_empty() && verbose {
                    let server_names: Vec<_> = needs_full.iter()
                        .map(|s| s.trim_start_matches("https://").trim_start_matches("http://").split('/').next().unwrap_or(s))
                        .collect();
                    eprintln!("  Full upload needed: {} (missing old tree)", server_names.join(", "));
                }
                Arc::new(needs_full)
            } else {
                Arc::new(Vec::new())
            };

            // Channel sends (data, is_from_old_tree) so worker knows which servers to target
            const CHANNEL_SIZE: usize = 100;
            const UPLOAD_CONCURRENCY: usize = 10;
            let (tx, rx) = mpsc::channel::<(Vec<u8>, bool)>(CHANNEL_SIZE);

            // Spawn upload workers
            let upload_handle = {
                let blossom = blossom.clone();
                let uploaded = Arc::clone(&uploaded);
                let skipped_server = Arc::clone(&skipped_server);
                let failed = Arc::clone(&failed);
                let processed = Arc::clone(&processed);
                let skipped_diff = Arc::clone(&skipped_diff);
                let has_old_tree = has_old_tree;
                let servers_needing_full = Arc::clone(&servers_needing_full);

                tokio::spawn(async move {
                    use futures::stream::StreamExt;
                    use tokio_stream::wrappers::ReceiverStream;

                    let stream = ReceiverStream::new(rx);
                    stream
                        .map(|(data, from_old_tree)| {
                            let blossom = &blossom;
                            let uploaded = Arc::clone(&uploaded);
                            let skipped_server = Arc::clone(&skipped_server);
                            let failed = Arc::clone(&failed);
                            let processed = Arc::clone(&processed);
                            let skipped_diff = Arc::clone(&skipped_diff);
                            let servers_needing_full = Arc::clone(&servers_needing_full);
                            async move {
                                // If from old tree and some servers need full upload, only upload to those
                                let result = if from_old_tree && !servers_needing_full.is_empty() {
                                    blossom.upload_to_all_servers(&data).await.map(|(h, c)| (h, c > 0))
                                } else {
                                    blossom.upload_if_missing(&data).await
                                };
                                match result {
                                    Ok((_, true)) => { uploaded.fetch_add(1, Ordering::Relaxed); }
                                    Ok((_, false)) => { skipped_server.fetch_add(1, Ordering::Relaxed); }
                                    Err(e) => {
                                        failed.fetch_add(1, Ordering::Relaxed);
                                        eprintln!("\n  Upload failed ({} bytes): {}", data.len(), e);
                                    }
                                }
                                let count = processed.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 || count % 10 == 0 {
                                    let diff_skipped = skipped_diff.load(Ordering::Relaxed);
                                    if has_old_tree && diff_skipped > 0 {
                                        eprint!("\r  Uploading: {} ({} new, {} unchanged, {} exist on server)",
                                            count,
                                            uploaded.load(Ordering::Relaxed),
                                            diff_skipped,
                                            skipped_server.load(Ordering::Relaxed));
                                    } else {
                                        eprint!("\r  Uploading: {} ({} new, {} exist)",
                                            count,
                                            uploaded.load(Ordering::Relaxed),
                                            skipped_server.load(Ordering::Relaxed));
                                    }
                                    let _ = std::io::stderr().flush();
                                }
                            }
                        })
                        .buffer_unordered(UPLOAD_CONCURRENCY)
                        .for_each(|_| async {})
                        .await;
                })
            };

            // Walk tree and send blobs to upload channel
            // Queue entries are (hash, optional decryption key)
            let mut visited: HashSet<[u8; 32]> = HashSet::new();
            let mut queue: Vec<([u8; 32], Option<[u8; 32]>)> = vec![(root_bytes, encryption_key.copied())];
            let mut queued_count = 0usize;

            eprint!("  Uploading: 0");
            let _ = std::io::stderr().flush();

            while let Some((hash, key)) = queue.pop() {
                if visited.contains(&hash) {
                    continue;
                }
                visited.insert(hash);

                // Check if this hash exists in old tree
                let from_old_tree = old_hashes.contains(&hash);

                // If from old tree and no servers need full upload, skip entirely
                if from_old_tree && servers_needing_full.is_empty() {
                    skipped_diff.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                // Load blob from store (stored encrypted)
                let data = match store.get_sync(&hash) {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("\n  Missing from local store: {}", hex::encode(hash));
                        continue;
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("\n  Store read error for {}: {}", hex::encode(hash), e);
                        continue;
                    }
                };

                // Decrypt if we have a key, then check if it's a tree node
                let plaintext = if let Some(k) = key {
                    match decrypt_chk(&data, &k) {
                        Ok(p) => p,
                        Err(_) => data.clone(), // Decryption failed, try as-is
                    }
                } else {
                    data.clone()
                };

                // Check if it's a tree node and queue children with their keys
                if let Some(node) = try_decode_tree_node(&plaintext) {
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            queue.push((link.hash, link.key));
                        }
                    }
                }

                // Send encrypted blob to upload channel (blossom stores ciphertext)
                if tx.send((data, from_old_tree)).await.is_err() {
                    break; // Channel closed
                }
                queued_count += 1;
                if queued_count % 100 == 0 {
                    eprint!("\r  Uploading: {} queued", queued_count);
                    let _ = std::io::stderr().flush();
                }
            }

            // Close channel and wait for uploads to complete
            drop(tx);
            let _ = upload_handle.await;

            let final_uploaded = uploaded.load(Ordering::Relaxed);
            let final_skipped_diff = skipped_diff.load(Ordering::Relaxed);
            let final_skipped_server = skipped_server.load(Ordering::Relaxed);
            let final_failed = failed.load(Ordering::Relaxed);
            let final_processed = processed.load(Ordering::Relaxed);

            // Final progress
            if has_old_tree {
                if final_failed > 0 {
                    eprint!("\r  Uploading: {} ({} new, {} unchanged, {} exist, {} FAILED)",
                        final_processed, final_uploaded, final_skipped_diff, final_skipped_server, final_failed);
                } else {
                    eprint!("\r  Uploading: {} ({} new, {} unchanged, {} exist)",
                        final_processed, final_uploaded, final_skipped_diff, final_skipped_server);
                }
            } else {
                if final_failed > 0 {
                    eprint!("\r  Uploading: {} ({} new, {} exist, {} FAILED)",
                        final_processed, final_uploaded, final_skipped_server, final_failed);
                } else {
                    eprint!("\r  Uploading: {} ({} new, {} exist)",
                        final_processed, final_uploaded, final_skipped_server);
                }
            }
            eprintln!();

            info!(
                "Blossom upload complete: {} uploaded, {} unchanged (diff), {} already on server, {} failed",
                final_uploaded, final_skipped_diff, final_skipped_server, final_failed
            );

            final_uploaded > 0 || final_skipped_server > 0 || final_skipped_diff > 0
        });

        // For now, we can't track per-server success because blossom client
        // returns on first successful server. Report all as succeeded if any worked.
        if success {
            BlossomResult {
                configured: configured.clone(),
                succeeded: configured,
                failed: vec![],
            }
        } else {
            BlossomResult {
                configured: configured.clone(),
                succeeded: vec![],
                failed: configured,
            }
        }
    }

    /// Collect all hashes reachable from a root hash by walking the merkle tree
    #[allow(dead_code)]
    fn collect_tree_hashes(&self, root_hash: &str) -> Result<Vec<[u8; 32]>> {
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let mut hashes = Vec::new();
        let mut visited: HashSet<[u8; 32]> = HashSet::new();

        // Parse root hash
        let root_bytes = hex::decode(root_hash)
            .context("Invalid root hash hex")?;
        if root_bytes.len() != 32 {
            bail!("Root hash must be 32 bytes");
        }
        let mut root: [u8; 32] = [0u8; 32];
        root.copy_from_slice(&root_bytes);

        let mut queue = vec![root];

        while let Some(hash) = queue.pop() {
            if visited.contains(&hash) {
                continue;
            }
            visited.insert(hash);
            hashes.push(hash);

            // Get blob data and check if it's a tree node
            if let Ok(Some(data)) = store.get_sync(&hash) {
                // Try to decode as tree node
                if let Some(node) = try_decode_tree_node(&data) {
                    // Queue all child hashes
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            queue.push(link.hash);
                        }
                    }
                }
                // If not a tree node, it's a leaf blob - already added to hashes
            }
        }

        debug!("Collected {} hashes from tree {}", hashes.len(), &root_hash[..12]);
        Ok(hashes)
    }

    /// List objects that need to be pushed (not on remote)
    fn list_objects_to_push(&self, sha: &str) -> Result<Vec<String>> {
        // Get all objects reachable from sha
        let output = Command::new("git")
            .args(["rev-list", "--objects", sha])
            .output()?;

        if !output.status.success() {
            bail!("Failed to list objects");
        }

        let mut objects = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            // Format: <sha> [path]
            if let Some(oid) = line.split_whitespace().next() {
                objects.push(oid.to_string());
            }
        }

        Ok(objects)
    }

    /// Read multiple git objects using git cat-file --batch
    /// Processes in batches to avoid pipe buffer deadlock
    fn read_git_objects_batch(&self, oids: &[String]) -> Result<Vec<(ObjectType, Vec<u8>)>> {
        use std::io::{BufRead, BufReader, Read, Write};

        if oids.is_empty() {
            return Ok(Vec::new());
        }

        let total = oids.len();
        let mut results = Vec::with_capacity(total);

        // Process in batches of 100 to avoid pipe buffer issues
        const BATCH_SIZE: usize = 100;

        for (batch_idx, batch) in oids.chunks(BATCH_SIZE).enumerate() {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;

            let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;

            // Write batch OIDs to stdin
            for oid in batch {
                writeln!(stdin, "{}", oid)?;
            }
            drop(stdin);

            // Read responses
            let mut reader = BufReader::new(stdout);

            for (i, oid) in batch.iter().enumerate() {
                let mut header = String::new();
                reader.read_line(&mut header)?;
                let header = header.trim();

                let parts: Vec<&str> = header.split_whitespace().collect();
                if parts.len() < 3 {
                    bail!("Object not found or invalid header for {}: {}", oid, header);
                }

                let obj_type = match parts[1] {
                    "blob" => ObjectType::Blob,
                    "tree" => ObjectType::Tree,
                    "commit" => ObjectType::Commit,
                    "tag" => ObjectType::Tag,
                    _ => bail!("Unknown object type: {}", parts[1]),
                };

                let size: usize = parts[2].parse()?;
                let mut content = vec![0u8; size];
                reader.read_exact(&mut content)?;

                // Read trailing newline
                let mut newline = [0u8; 1];
                reader.read_exact(&mut newline)?;

                results.push((obj_type, content));

                // Progress indicator
                let done = batch_idx * BATCH_SIZE + i + 1;
                if done == 1 || done % 100 == 0 || done == total {
                    eprint!("\r  Reading objects: {}/{}", done, total);
                    let _ = std::io::stderr().flush();
                }
            }

            child.wait()?;
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PUBKEY: &str = "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";

    fn create_test_helper() -> Option<RemoteHelper> {
        let config = Config::default();
        RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
    }

    #[test]
    fn test_capabilities() {
        let Some(helper) = create_test_helper() else {
            return; // Skip if storage can't be created
        };

        let caps = helper.capabilities();
        assert!(caps.contains(&"fetch".to_string()));
        assert!(caps.contains(&"push".to_string()));
        assert!(caps.contains(&"option".to_string()));
        // Should end with empty line
        assert_eq!(caps.last(), Some(&String::new()));
    }

    #[test]
    fn test_handle_capabilities_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("capabilities").unwrap();
        assert!(result.is_some());
        let caps = result.unwrap();
        assert!(caps.contains(&"fetch".to_string()));
        assert!(caps.contains(&"push".to_string()));
    }

    #[test]
    fn test_handle_list_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // list should return refs (empty for new repo)
        let result = helper.handle_command("list").unwrap();
        assert!(result.is_some());
        let lines = result.unwrap();
        // Should end with empty line
        assert_eq!(lines.last(), Some(&String::new()));
    }

    #[test]
    fn test_handle_list_for_push_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("list for-push").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_handle_option_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("option verbosity 1").unwrap();
        assert!(result.is_some());
        let lines = result.unwrap();
        assert!(lines.contains(&"unsupported".to_string()));
    }

    #[test]
    fn test_handle_unknown_command() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("unknown-command").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_handle_empty_line_exits() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        assert!(!helper.should_exit());
        let _ = helper.handle_command("").unwrap();
        assert!(helper.should_exit());
    }

    #[test]
    fn test_queue_fetch() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Queue a fetch
        let result = helper.handle_command("fetch abc123def456 refs/heads/main").unwrap();
        assert!(result.is_none()); // fetch queues, doesn't respond immediately

        assert_eq!(helper.fetch_specs.len(), 1);
        assert_eq!(helper.fetch_specs[0].sha, "abc123def456");
        assert_eq!(helper.fetch_specs[0].name, "refs/heads/main");
    }

    #[test]
    fn test_queue_multiple_fetches() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper.handle_command("fetch abc123 refs/heads/main").unwrap();
        helper.handle_command("fetch def456 refs/heads/feature").unwrap();

        assert_eq!(helper.fetch_specs.len(), 2);
    }

    #[test]
    fn test_queue_fetch_invalid() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Missing name
        let result = helper.handle_command("fetch abc123");
        assert!(result.is_err());
    }

    #[test]
    fn test_queue_push() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        let result = helper.handle_command("push refs/heads/main:refs/heads/main").unwrap();
        assert!(result.is_none()); // push queues, doesn't respond immediately

        assert_eq!(helper.push_specs.len(), 1);
        assert_eq!(helper.push_specs[0].src, "refs/heads/main");
        assert_eq!(helper.push_specs[0].dst, "refs/heads/main");
        assert!(!helper.push_specs[0].force);
    }

    #[test]
    fn test_queue_force_push() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper.handle_command("push +refs/heads/main:refs/heads/main").unwrap();

        assert_eq!(helper.push_specs.len(), 1);
        assert!(helper.push_specs[0].force);
    }

    #[test]
    fn test_queue_delete_push() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Empty src means delete
        helper.handle_command("push :refs/heads/old-branch").unwrap();

        assert_eq!(helper.push_specs.len(), 1);
        assert_eq!(helper.push_specs[0].src, "");
        assert_eq!(helper.push_specs[0].dst, "refs/heads/old-branch");
    }

    #[test]
    fn test_queue_push_invalid() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Missing colon separator
        let result = helper.handle_command("push refs/heads/main");
        assert!(result.is_err());
    }

    #[test]
    fn test_push_spec_parsing() {
        // Test internal PushSpec parsing via queue_push
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Normal push
        helper.queue_push("src:dst").unwrap();
        assert_eq!(helper.push_specs[0].src, "src");
        assert_eq!(helper.push_specs[0].dst, "dst");
        assert!(!helper.push_specs[0].force);

        helper.push_specs.clear();

        // Force push
        helper.queue_push("+src:dst").unwrap();
        assert!(helper.push_specs[0].force);
        assert_eq!(helper.push_specs[0].src, "src");

        helper.push_specs.clear();

        // Delete (empty src)
        helper.queue_push(":dst").unwrap();
        assert_eq!(helper.push_specs[0].src, "");
        assert_eq!(helper.push_specs[0].dst, "dst");
    }

    #[test]
    fn test_fetch_spec_parsing() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper.queue_fetch("abc123def456789 refs/heads/main").unwrap();

        assert_eq!(helper.fetch_specs[0].sha, "abc123def456789");
        assert_eq!(helper.fetch_specs[0].name, "refs/heads/main");
    }

    #[test]
    fn test_fetch_spec_with_tag() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        helper.queue_fetch("abc123 refs/tags/v1.0.0").unwrap();
        assert_eq!(helper.fetch_specs[0].name, "refs/tags/v1.0.0");
    }

    #[test]
    fn test_should_exit_initially_false() {
        let Some(helper) = create_test_helper() else {
            return;
        };

        assert!(!helper.should_exit());
    }

    #[test]
    fn test_get_hashtree_data_dir() {
        let dir = get_hashtree_data_dir();
        assert!(dir.ends_with("data"));
        assert!(dir.to_string_lossy().contains(".hashtree"));
    }

    #[test]
    fn test_command_parsing_with_spaces() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Commands are split by first space only
        let result = helper.handle_command("option verbosity 1").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_list_clears_remote_refs() {
        let Some(mut helper) = create_test_helper() else {
            return;
        };

        // Add some dummy refs
        helper.remote_refs.insert("refs/heads/old".to_string(), "abc".to_string());

        // list should clear and repopulate
        helper.handle_command("list").unwrap();

        // For empty repo, refs should be empty
        assert!(helper.remote_refs.is_empty());
    }
}
