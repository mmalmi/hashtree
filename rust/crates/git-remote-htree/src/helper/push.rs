use super::progress::emit_upload_progress;
use super::storage_support::{build_repo_viewer_url, get_hashtree_data_dir, queue_hash_if_new};
use super::{fetch_progress_interval, upload_progress, AncestorCheck, PushSpec, RemoteHelper};
use crate::git::progress::RepoTreeBuildProgress;
use crate::git::refs::Ref;
use crate::nostr_client::{
    resolve_identity, BlossomResult, PullRequestStateFilter, RepoAnnouncementOptions,
};
use crate::runtime::{block_on_result, new_multi_thread_runtime};
use anyhow::{bail, Context, Result};
use hashtree_core::{HashTree, Store};
use nostr_sdk::prelude::{PublicKey, ToBech32};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

const SERVER_COVERAGE_SAMPLE_SIZE: usize = 32;
const UPLOAD_CHECK_BATCH_SIZE: usize = 10_000;
const GIT_BATCH_UPLOAD_TARGET_BYTES: usize = 8 * 1024 * 1024;
const BATCH_UPLOAD_RETRIES: u32 = 4;
const DEFAULT_GIT_PACK_CHECKPOINT_MIN_OBJECTS: usize = 4_096;
const GIT_PACK_CHECKPOINT_MIN_OBJECTS_ENV: &str = "HTREE_GIT_PACK_CHECKPOINT_MIN_OBJECTS";

struct ServerUploadPresence {
    present: HashSet<[u8; 32]>,
    complete: bool,
}

struct PendingUpload {
    hash: [u8; 32],
    data: Vec<u8>,
    from_old_tree: bool,
    force_all_servers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForkSourceRepo {
    pubkey_hex: String,
    repo_name: String,
    canonical_url: String,
}

#[derive(Clone)]
struct UploadCounters {
    uploaded: Arc<AtomicUsize>,
    skipped_diff: Arc<AtomicUsize>,
    skipped_server: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    discovered_total: Arc<AtomicUsize>,
}

pub(super) struct GitPackCheckpointPlan {
    pub(super) packs: Vec<GitPackCheckpointPackPlan>,
    pub(super) covered_objects: HashSet<String>,
}

pub(super) struct GitPackCheckpointPackPlan {
    pub(super) tip: String,
    pub(super) exclude_tip: Option<String>,
}

struct RepoTreeProgressReporter {
    stop: Arc<AtomicBool>,
    printed: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl RepoTreeProgressReporter {
    fn start(label: &str, progress: RepoTreeBuildProgress) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let printed = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_printed = Arc::clone(&printed);
        let label = label.to_string();

        let handle = thread::spawn(move || {
            let interval = fetch_progress_interval();
            while !thread_stop.load(Ordering::Relaxed) {
                let mut slept = Duration::ZERO;
                while slept < interval {
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let step = Duration::from_millis(100).min(interval - slept);
                    thread::sleep(step);
                    slept += step;
                }
                if thread_stop.load(Ordering::Relaxed) {
                    return;
                }
                eprintln!("{}", progress.snapshot().format_for_label(&label));
                thread_printed.store(true, Ordering::Relaxed);
            }
        });

        Self {
            stop,
            printed,
            handle,
        }
    }

    fn finish<E: std::fmt::Display>(
        self,
        label: &str,
        progress: &RepoTreeBuildProgress,
        error: Option<&E>,
    ) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();

        if !self.printed.load(Ordering::Relaxed) {
            return;
        }

        match error {
            Some(err) => {
                eprintln!("  {}: failed ({})", label, err);
            }
            None => {
                eprintln!("{}", progress.snapshot().format_for_label(label));
            }
        }
    }
}

fn effective_upload_concurrency(server_count: usize, configured: usize) -> usize {
    let configured = configured.max(1);
    if server_count == 0 {
        1
    } else {
        configured
    }
}

fn git_batch_upload_target_bytes() -> usize {
    GIT_BATCH_UPLOAD_TARGET_BYTES
        .min(hashtree_blossom::BATCH_UPLOAD_MAX_BYTES)
        .max(1)
}

fn batch_upload_retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(3))
}

pub(super) fn ensure_blossom_publish_ready(result: &BlossomResult) -> Result<()> {
    if !result.local_complete {
        bail!(
            "Failed to prepare complete repo tree in local hashtree store; not publishing incomplete root"
        );
    }

    if result.degraded {
        let failed = if result.failed.is_empty() {
            "unknown write server(s)".to_string()
        } else {
            result.failed.join(", ")
        };
        bail!(
            "Remote Blossom replication incomplete ({}); not publishing root that clients may not be able to read",
            failed
        );
    }

    Ok(())
}

fn root_availability_error(root_hash: &str, unavailable: &[String]) -> anyhow::Error {
    let servers = if unavailable.is_empty() {
        "no configured write server".to_string()
    } else {
        unavailable.join(", ")
    };
    anyhow::anyhow!(
        "Uploaded repo root {} is not readable from configured Blossom write server(s): {}; not publishing root",
        &root_hash[..12.min(root_hash.len())],
        servers
    )
}

fn git_pack_checkpoint_min_objects() -> usize {
    std::env::var(GIT_PACK_CHECKPOINT_MIN_OBJECTS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_GIT_PACK_CHECKPOINT_MIN_OBJECTS)
}

fn unique_git_pack_temp_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("htree-git-pack-{}-{}", std::process::id(), nanos))
}

pub(super) fn queue_links_for_diff_upload(
    queue: &mut Vec<([u8; 32], Option<[u8; 32]>)>,
    queued: &mut HashSet<[u8; 32]>,
    links: &[hashtree_core::Link],
    old_hashes: &HashSet<[u8; 32]>,
    prune_known_subtrees: bool,
    discovered_total: &std::sync::atomic::AtomicUsize,
) {
    use std::sync::atomic::Ordering;

    for link in links {
        if prune_known_subtrees && old_hashes.contains(&link.hash) {
            continue;
        }
        if queue_hash_if_new(queue, queued, link.hash, link.key) {
            discovered_total.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn collect_complete_hashes<S: Store>(
    tree: &HashTree<S>,
    root: &hashtree_core::Cid,
    concurrency: usize,
) -> Result<HashSet<[u8; 32]>, hashtree_core::HashTreeError> {
    let hashes = hashtree_core::collect_hashes(tree, root, concurrency).await?;
    let store = tree.get_store();
    for hash in &hashes {
        match store.has(hash).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(hashtree_core::HashTreeError::MissingChunk(hex::encode(
                    hash,
                )));
            }
            Err(err) => {
                return Err(hashtree_core::HashTreeError::Store(err.to_string()));
            }
        }
    }
    Ok(hashes)
}

async fn check_upload_presence_on_servers(
    blossom: &hashtree_blossom::BlossomClient,
    servers: &[String],
    hashes: &HashSet<[u8; 32]>,
) -> Option<ServerUploadPresence> {
    if servers.is_empty() || hashes.is_empty() {
        return None;
    }

    let mut sorted_hashes: Vec<[u8; 32]> = hashes.iter().copied().collect();
    sorted_hashes.sort_unstable();
    let hash_hexes: Vec<String> = sorted_hashes.iter().map(hex::encode).collect();
    let mut present = HashSet::new();
    let mut checked_servers = 0usize;

    for server in servers {
        let Some(server_present) = blossom.check_uploads_on_server(&hash_hexes, server).await
        else {
            debug!("Blossom upload check unavailable for {}", server);
            continue;
        };
        checked_servers += 1;
        for hash_hex in server_present {
            let Ok(bytes) = hex::decode(&hash_hex) else {
                continue;
            };
            if bytes.len() != 32 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&bytes);
            present.insert(hash);
        }
    }

    (checked_servers > 0).then_some(ServerUploadPresence {
        present,
        complete: checked_servers == servers.len(),
    })
}

fn record_skipped_candidate(counters: &UploadCounters, from_old_tree: bool) {
    if from_old_tree {
        counters.skipped_diff.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.skipped_server.fetch_add(1, Ordering::Relaxed);
    }

    counters.completed.fetch_add(1, Ordering::Relaxed);
}

fn record_batch_upload_result(counters: &UploadCounters, attempted: usize, uploaded: usize) {
    let uploaded = uploaded.min(attempted);
    let existing = attempted.saturating_sub(uploaded);
    counters.uploaded.fetch_add(uploaded, Ordering::Relaxed);
    counters
        .skipped_server
        .fetch_add(existing, Ordering::Relaxed);

    counters.completed.fetch_add(attempted, Ordering::Relaxed);
}

fn record_batch_upload_failure(counters: &UploadCounters, attempted: usize) {
    counters.failed.fetch_add(attempted, Ordering::Relaxed);
    counters.completed.fetch_add(attempted, Ordering::Relaxed);
}

fn upload_progress_from_counters(
    counters: &UploadCounters,
    discovery_complete: &AtomicBool,
    has_old_tree: bool,
) -> super::progress::UploadProgress {
    let discovered = counters.discovered_total.load(Ordering::Relaxed);
    upload_progress(
        counters.completed.load(Ordering::Relaxed),
        discovered,
        discovery_complete
            .load(Ordering::Relaxed)
            .then_some(discovered),
        counters.uploaded.load(Ordering::Relaxed),
        counters.skipped_diff.load(Ordering::Relaxed),
        counters.skipped_server.load(Ordering::Relaxed),
        counters.failed.load(Ordering::Relaxed),
        has_old_tree,
    )
}

fn spawn_periodic_upload_progress(
    counters: UploadCounters,
    discovery_complete: Arc<AtomicBool>,
    has_old_tree: bool,
    mut done: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(fetch_progress_interval()) => {
                    emit_upload_progress(upload_progress_from_counters(
                        &counters,
                        &discovery_complete,
                        has_old_tree,
                    ));
                }
                changed = done.changed() => {
                    if changed.is_err() || *done.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

async fn enqueue_pending_upload(
    tx: &tokio::sync::mpsc::Sender<([u8; 32], Vec<u8>, bool, bool, bool)>,
    item: PendingUpload,
    head_fallback: bool,
) -> bool {
    tx.send((
        item.hash,
        item.data,
        item.from_old_tree,
        item.force_all_servers,
        head_fallback,
    ))
    .await
    .is_ok()
}

async fn upload_one_pending_batch_to_server(
    batch: &[PendingUpload],
    blossom: &hashtree_blossom::BlossomClient,
    server: &str,
    counters: &UploadCounters,
) -> Result<Option<()>> {
    let items: Vec<_> = batch
        .iter()
        .map(|item| {
            hashtree_blossom::BatchUploadItem::new(hex::encode(item.hash), item.data.clone())
        })
        .collect();

    let mut last_error = String::new();
    for attempt in 0..BATCH_UPLOAD_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(batch_upload_retry_delay(attempt - 1)).await;
        }

        match blossom.upload_batch_to_server(server, &items).await {
            Ok(Some(result)) => {
                record_batch_upload_result(counters, batch.len(), result.uploaded);
                return Ok(Some(()));
            }
            Ok(None) => return Ok(None),
            Err(err) => {
                last_error = err.to_string();
                debug!(
                    "Blossom batch upload attempt {}/{} failed on {}: {}",
                    attempt + 1,
                    BATCH_UPLOAD_RETRIES,
                    server,
                    last_error
                );
            }
        }
    }

    Err(anyhow::anyhow!(
        "Blossom batch upload failed on {} after {} attempts: {}",
        server,
        BATCH_UPLOAD_RETRIES,
        last_error
    ))
}

async fn upload_pending_with_single_server_batches(
    items: Vec<PendingUpload>,
    blossom: &hashtree_blossom::BlossomClient,
    server: &str,
    counters: &UploadCounters,
) -> Result<Vec<PendingUpload>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let mut fallback = Vec::new();
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let mut batch_supported = true;
    let batch_target_bytes = git_batch_upload_target_bytes();

    for item in items {
        if !batch_supported {
            fallback.push(item);
            continue;
        }

        let item_len = item.data.len();
        let would_overflow = !batch.is_empty()
            && (batch.len() >= hashtree_blossom::BATCH_UPLOAD_MAX_BLOBS
                || batch_bytes.saturating_add(item_len) > batch_target_bytes);
        if would_overflow {
            match upload_one_pending_batch_to_server(&batch, blossom, server, counters).await {
                Ok(Some(())) => {
                    batch.clear();
                    batch_bytes = 0;
                }
                Ok(None) => {
                    fallback.append(&mut batch);
                    batch_bytes = 0;
                    batch_supported = false;
                    fallback.push(item);
                    continue;
                }
                Err(err) => {
                    record_batch_upload_failure(counters, batch.len());
                    eprintln!("\n  Batch upload failed ({} blobs): {}", batch.len(), err);
                    return Err(err);
                }
            }
        }

        batch_bytes = batch_bytes.saturating_add(item_len);
        batch.push(item);
    }

    if batch_supported && !batch.is_empty() {
        match upload_one_pending_batch_to_server(&batch, blossom, server, counters).await {
            Ok(Some(())) => {}
            Ok(None) => {
                fallback.append(&mut batch);
            }
            Err(err) => {
                record_batch_upload_failure(counters, batch.len());
                eprintln!("\n  Batch upload failed ({} blobs): {}", batch.len(), err);
                return Err(err);
            }
        }
    } else {
        fallback.append(&mut batch);
    }

    Ok(fallback)
}

async fn flush_pending_uploads(
    pending: &mut Vec<PendingUpload>,
    blossom: &hashtree_blossom::BlossomClient,
    all_servers: &[String],
    use_upload_check: bool,
    repairing_server_tree: bool,
    upload_check_supported: &mut bool,
    tx: &tokio::sync::mpsc::Sender<([u8; 32], Vec<u8>, bool, bool, bool)>,
    counters: &UploadCounters,
) -> bool {
    if pending.is_empty() {
        return true;
    }

    let mut present = HashSet::new();
    let mut checked_all_servers = false;
    if use_upload_check && *upload_check_supported {
        let hashes: HashSet<[u8; 32]> = pending.iter().map(|item| item.hash).collect();
        match check_upload_presence_on_servers(blossom, all_servers, &hashes).await {
            Some(presence) => {
                checked_all_servers = presence.complete;
                present = presence.present;
            }
            None => {
                *upload_check_supported = false;
            }
        }
    }

    let head_fallback = use_upload_check && !checked_all_servers && !repairing_server_tree;
    let mut to_upload = Vec::new();
    for item in pending.drain(..) {
        if present.contains(&item.hash) {
            record_skipped_candidate(counters, item.from_old_tree);
            continue;
        }

        to_upload.push(item);
    }

    if all_servers.len() == 1 && !to_upload.is_empty() {
        let server = &all_servers[0];
        let mut batchable = Vec::new();
        for item in to_upload {
            if item.data.len() > hashtree_blossom::BATCH_UPLOAD_MAX_BYTES {
                if !enqueue_pending_upload(tx, item, head_fallback).await {
                    return false;
                }
            } else {
                batchable.push(item);
            }
        }

        match upload_pending_with_single_server_batches(batchable, blossom, server, counters).await
        {
            Ok(fallback) => {
                to_upload = fallback;
            }
            Err(_) => return false,
        }
    }

    for item in to_upload {
        if !enqueue_pending_upload(tx, item, head_fallback).await {
            return false;
        }
    }

    true
}

async fn upload_block_to_file_servers(
    blossom: &hashtree_blossom::BlossomClient,
    data: &[u8],
    from_old_tree: bool,
    force_all_servers: bool,
    servers_needing_full: &[String],
) -> std::result::Result<(String, bool), hashtree_blossom::BlossomError> {
    let write_server_count = blossom.write_servers().len();
    if force_all_servers || (!from_old_tree && write_server_count > 1) {
        if write_server_count <= 1 {
            blossom.upload_if_missing(data).await
        } else {
            blossom
                .upload_to_any_selected_server(data, blossom.write_servers())
                .await
        }
    } else if from_old_tree && !servers_needing_full.is_empty() {
        blossom
            .upload_to_any_selected_server(data, servers_needing_full)
            .await
    } else {
        blossom.upload_if_missing(data).await
    }
}

impl RemoteHelper {
    fn upload_concurrency(&self, server_count: usize) -> usize {
        effective_upload_concurrency(server_count, self.config.blossom.upload_concurrency)
    }

    pub(super) fn verify_root_available_on_write_server(&self, root_hash: &str) -> Result<()> {
        let blossom = self.nostr.blossom().clone();
        let write_servers = blossom.write_servers().to_vec();
        if write_servers.is_empty() {
            return Err(root_availability_error(root_hash, &[]));
        }

        block_on_result(async {
            let mut unavailable = Vec::new();
            for server in &write_servers {
                match blossom.check_on_server(root_hash, server).await {
                    hashtree_blossom::BlobAvailability::Present => return Ok(()),
                    hashtree_blossom::BlobAvailability::Missing
                    | hashtree_blossom::BlobAvailability::Unknown => {
                        unavailable.push(server.clone());
                    }
                }
            }
            Err(root_availability_error(root_hash, &unavailable))
        })
    }

    fn build_tree_with_progress(&self, label: &str) -> Result<hashtree_core::Cid> {
        let progress = RepoTreeBuildProgress::new();
        let reporter = RepoTreeProgressReporter::start(label, progress.clone());
        let result = self.storage.build_tree_with_progress(&progress);
        reporter.finish(label, &progress, result.as_ref().err());
        Ok(result?)
    }

    fn build_tree_with_base_progress<S: Store>(
        &self,
        label: &str,
        base_tree: Option<&HashTree<S>>,
        base_root: Option<&hashtree_core::Cid>,
        base_tree_sha: Option<&str>,
    ) -> Result<hashtree_core::Cid> {
        let progress = RepoTreeBuildProgress::new();
        let reporter = RepoTreeProgressReporter::start(label, progress.clone());
        let result = self.storage.build_tree_with_base_objects_with_progress(
            base_tree,
            base_root,
            base_tree_sha,
            &progress,
        );
        reporter.finish(label, &progress, result.as_ref().err());
        Ok(result?)
    }

    fn build_tree_with_cached_remote_root(
        &self,
        label: &str,
        delta_base: Option<&str>,
    ) -> Result<Option<hashtree_core::Cid>> {
        let Some(root_hash) = self.nostr.get_cached_root_hash(&self.repo_name).cloned() else {
            return Ok(None);
        };

        if self.is_slow() {
            eprintln!("  {label}...");
        }

        let encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .copied();
        let (cached_tree, _) = self.build_cached_fetch_tree()?;
        let root_bytes = hex::decode(&root_hash).context("Invalid cached root hash hex")?;
        let root_arr: [u8; 32] = root_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cached root hash must be 32 bytes"))?;
        let cached_root_cid = hashtree_core::Cid {
            hash: root_arr,
            key: encryption_key,
        };

        let base_tree_sha = delta_base
            .map(Self::resolve_tree_sha)
            .transpose()
            .context("resolve delta base tree")?;

        self.build_tree_with_base_progress(
            label,
            Some(&cached_tree),
            Some(&cached_root_cid),
            base_tree_sha.as_deref(),
        )
        .map(Some)
    }

    fn cached_remote_root_has_git_pack_checkpoint(&self) -> Result<Option<bool>> {
        let Some(root_hash) = self.nostr.get_cached_root_hash(&self.repo_name).cloned() else {
            return Ok(None);
        };
        let encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .copied();
        let (cached_tree, _) = self.build_cached_fetch_tree()?;
        let root_bytes = hex::decode(&root_hash).context("Invalid cached root hash hex")?;
        let root_arr: [u8; 32] = root_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cached root hash must be 32 bytes"))?;
        let cached_root_cid = hashtree_core::Cid {
            hash: root_arr,
            key: encryption_key,
        };

        self.storage
            .tree_root_has_git_pack_checkpoint(&cached_tree, &cached_root_cid)
            .map(Some)
            .map_err(Into::into)
    }

    fn cached_remote_root_pack_checkpoint_available(&self) -> bool {
        match self.cached_remote_root_has_git_pack_checkpoint() {
            Ok(Some(has_checkpoint)) => has_checkpoint,
            Ok(None) => false,
            Err(err) => {
                warn!(
                    "Could not inspect cached remote git pack checkpoint; rebuilding checkpoint: {}",
                    err
                );
                false
            }
        }
    }

    pub(super) fn delta_base_for_push(
        &self,
        sha: &str,
        force_push: bool,
        remote_tip_sha: Option<&str>,
    ) -> Option<String> {
        if force_push {
            return None;
        }

        if let Some(remote_tip) = remote_tip_sha {
            return Some(remote_tip.to_string());
        }

        self.find_existing_remote_delta_base(sha)
    }

    fn find_existing_remote_delta_base(&self, sha: &str) -> Option<String> {
        let mut heads = Vec::new();
        let mut other_refs = Vec::new();
        for (ref_name, ref_sha) in &self.remote_refs {
            if ref_sha == sha {
                return Some(ref_sha.clone());
            }
            if ref_name.starts_with("refs/heads/") {
                heads.push(ref_sha.clone());
            } else {
                other_refs.push(ref_sha.clone());
            }
        }

        heads.sort();
        heads.dedup();
        other_refs.sort();
        other_refs.dedup();

        let mut candidates = heads;
        candidates.extend(other_refs);
        for candidate in candidates {
            match self.check_ancestor(&candidate, sha) {
                AncestorCheck::Ancestor => return Some(candidate),
                AncestorCheck::NotAncestor | AncestorCheck::Unknown(_) => {}
            }
        }

        None
    }

    fn repair_delta_tree_build(
        &mut self,
        sha: &str,
        dst_ref: &str,
        base: &str,
        reason_label: &str,
        reason: String,
    ) -> Result<hashtree_core::Cid> {
        eprintln!(
            "  {reason_label} ({}); hydrating existing remote objects from cached root",
            reason
        );
        debug!(
            "{} for {} via {}: {}. Hydrating cached remote objects before full local import.",
            reason_label, dst_ref, base, reason
        );

        if let Some(root_hash) = self.nostr.get_cached_root_hash(&self.repo_name).cloned() {
            let existing_objects = self.fetch_all_git_objects(&root_hash)?;
            eprintln!(
                "  Importing {} cached remote object(s)",
                existing_objects.len()
            );
            for (oid, content) in existing_objects {
                self.storage.import_compressed_object(&oid, content)?;
            }
        }

        let mut hydrated_missing = HashSet::new();
        let post_hydration_err = loop {
            let retry = self
                .build_tree_with_cached_remote_root(
                    "Retrying repo tree after cached-object hydration",
                    Some(base),
                )
                .and_then(|root| {
                    root.ok_or_else(|| {
                        anyhow::anyhow!("cached remote root disappeared after hydration")
                    })
                });

            match retry {
                Ok(root_cid) => return Ok(root_cid),
                Err(err) => {
                    let Some(missing_oid) = Self::missing_object_id_from_error(&err) else {
                        break err;
                    };
                    if !hydrated_missing.insert(missing_oid.clone()) || hydrated_missing.len() > 512
                    {
                        break err;
                    }

                    eprintln!(
                        "  Hydrating missing local git object {} for cached-root merge",
                        &missing_oid[..12]
                    );
                    let objects_with_content =
                        self.read_git_objects_batch(std::slice::from_ref(&missing_oid))?;
                    eprintln!();
                    Self::write_objects_to_local_store(&self.storage, objects_with_content)?;
                }
            }
        };

        {
            eprintln!(
                "  Cached-root hydration still incomplete ({}); falling back to full local import",
                post_hydration_err
            );
            debug!(
                    "Cached remote hydration still incomplete for {} via {}: {}. Falling back to full local import.",
                    dst_ref, base, post_hydration_err
                );

            eprintln!("  Listing objects...");
            let objects = self.list_objects_to_push(sha, &[])?;
            eprintln!("  Listed {} object(s)", objects.len());

            let objects_with_content = self.read_git_objects_batch(&objects)?;

            eprintln!("  Writing to local store...");
            Self::write_objects_to_local_store(&self.storage, objects_with_content)?;

            self.build_tree_with_progress("Building repo tree from repaired local store")
        }
    }

    fn missing_object_id_from_error(err: &anyhow::Error) -> Option<String> {
        for cause in err.chain() {
            if let Some(crate::git::error::Error::ObjectNotFound(oid)) =
                cause.downcast_ref::<crate::git::error::Error>()
            {
                if Self::is_hex_object_id(oid) {
                    return Some(oid.clone());
                }
            }

            let message = cause.to_string();
            if let Some(oid) = message.strip_prefix("Object not found: ") {
                if Self::is_hex_object_id(oid) {
                    return Some(oid.to_string());
                }
            }
        }

        None
    }

    /// Queue a push operation
    pub(super) fn queue_push(&mut self, arg: &str) -> Result<()> {
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
    pub(super) fn execute_push(&mut self) -> Result<Option<Vec<String>>> {
        self.start_op(); // Start timing for conditional verbose logging
        debug!(refs_count = self.push_specs.len(), "execute_push called");
        info!("Pushing {} refs", self.push_specs.len());

        // First, load existing refs and objects from remote to preserve other branches
        let has_force_push = self.push_specs.iter().any(|s| s.force);
        debug!(
            force = has_force_push,
            "About to call load_existing_remote_state"
        );

        if let Err(e) = self.load_existing_remote_state() {
            let err_str = e.to_string();
            let is_access_error = err_str.contains("link-visible")
                || err_str.contains("private")
                || err_str.contains("secret key");
            let is_likely_new_repo = err_str.contains("No root hash")
                || err_str.contains("not found")
                || err_str.contains("timeout");

            if is_access_error {
                debug!("Cannot access existing repo (visibility change): {}", e);
            } else if has_force_push {
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Proceeding with force push (may overwrite other branches)");
            } else if is_likely_new_repo {
                debug!("Error loading remote state (likely new repo): {}", e);
                info!(
                    "Could not load existing remote state: {} (likely new repo)",
                    e
                );
            } else {
                eprintln!("  Warning: Could not load existing remote state: {}", e);
                eprintln!("  Other branches may be lost. Use 'git push --force' to override.");
                eprintln!("  Or check your network connection and try again.");
            }
        }

        let mut results = Vec::new();
        let mut pushed_refs: Vec<(String, String)> = Vec::new();
        let specs: Vec<_> = std::mem::take(&mut self.push_specs);

        for spec in specs {
            debug!(
                "Pushing {} -> {} (force={})",
                spec.src, spec.dst, spec.force
            );

            let sha = if spec.src.is_empty() {
                String::new()
            } else {
                self.resolve_ref(&spec.src)?
            };

            if sha.is_empty() {
                match self.storage.delete_ref(&spec.dst) {
                    Ok(_) => {
                        self.nostr.delete_ref(&self.repo_name, &spec.dst)?;
                        results.push(format!("ok {}", spec.dst));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            } else {
                if !spec.force {
                    if let Some(remote_sha) = self.remote_refs.get(&spec.dst) {
                        match self.check_ancestor(remote_sha, &sha) {
                            AncestorCheck::Ancestor => {}
                            AncestorCheck::NotAncestor => {
                                results.push(format!(
                                    "error {} non-fast-forward (use --force to override)",
                                    spec.dst
                                ));
                                eprintln!(
                                    "  Rejected: {} has commits you don't have. Pull first or use --force.",
                                    spec.dst
                                );
                                eprintln!("  remote: {}", remote_sha);
                                eprintln!("  local : {}", sha);
                                continue;
                            }
                            AncestorCheck::Unknown(reason) => {
                                results.push(format!(
                                    "error {} fast-forward-check-failed (use --force to override)",
                                    spec.dst
                                ));
                                eprintln!("  Rejected: {} fast-forward check failed.", spec.dst);
                                eprintln!("  Could not verify ancestry between:");
                                eprintln!("    remote: {}", remote_sha);
                                eprintln!("    local : {}", sha);
                                eprintln!("  merge-base error: {}", reason);
                                continue;
                            }
                        }
                    }
                }

                let remote_tip = self.remote_refs.get(&spec.dst).cloned();
                match self.push_objects(&sha, &spec.dst, spec.force, remote_tip.as_deref()) {
                    Ok(()) => {
                        results.push(format!("ok {}", spec.dst));
                        pushed_refs.push((spec.dst, sha));
                    }
                    Err(e) => results.push(format!("error {} {}", spec.dst, e)),
                }
            }
        }

        if self.nostr.can_sign() && !pushed_refs.is_empty() {
            self.detect_and_mark_merged_prs(&pushed_refs);
        }

        results.push(String::new());
        Ok(Some(results))
    }

    /// Load existing refs and objects from remote before pushing
    /// This preserves branches that aren't being pushed
    pub(super) fn load_existing_remote_state(&mut self) -> Result<()> {
        let data_dir = get_hashtree_data_dir();
        self.detail(&format!(
            "  Loading existing remote state... (data_dir: {:?})",
            data_dir
        ));

        let (refs, root_hash, _encryption_key) =
            self.nostr.fetch_refs_with_root(&self.repo_name)?;

        if refs.is_empty() {
            self.detail("  No existing refs found (new repository)");
            return Ok(());
        }

        self.detail(&format!("  Found {} existing refs", refs.len()));
        self.remote_refs.clear();
        for (ref_name, ref_value) in &refs {
            if ref_name.starts_with("refs/") && !ref_value.starts_with("ref: ") {
                self.remote_refs.insert(ref_name.clone(), ref_value.clone());
            }
        }

        for (ref_name, ref_value) in &refs {
            let is_being_pushed = self.push_specs.iter().any(|s| s.dst == *ref_name);
            if !is_being_pushed {
                self.storage.import_ref(ref_name, ref_value)?;
                debug!(
                    "Imported existing ref: {} -> {}",
                    ref_name,
                    &ref_value[..12.min(ref_value.len())]
                );
            }
        }

        let preserved_refs: Vec<(String, String)> = refs
            .iter()
            .filter(|(ref_name, ref_value)| {
                ref_name.starts_with("refs/")
                    && !ref_value.starts_with("ref: ")
                    && !self.push_specs.iter().any(|spec| spec.dst == **ref_name)
            })
            .map(|(ref_name, ref_value)| (ref_name.clone(), ref_value.clone()))
            .collect();

        if preserved_refs.is_empty() {
            self.detail("  No untouched direct refs to preserve");
            self.detail("  Remote state loaded");
            return Ok(());
        }

        if self.import_preserved_remote_objects_from_local_git(&preserved_refs)? {
            self.detail("  Reused preserved remote objects from local git");
        } else if let Some(root) = root_hash {
            self.detail(
                "  Falling back to remote object import for preserved refs not available locally",
            );
            let objects = self.fetch_all_git_objects(&root)?;
            self.detail(&format!("  Importing {} existing objects", objects.len()));

            for (oid, content) in objects {
                self.storage.import_compressed_object(&oid, content)?;
            }
        } else {
            bail!("No root hash found for repository - cannot preserve untouched refs");
        }

        self.detail("  Remote state loaded");
        Ok(())
    }
    pub(super) fn import_preserved_remote_objects_from_local_git(
        &self,
        preserved_refs: &[(String, String)],
    ) -> Result<bool> {
        let mut include_shas: Vec<String> =
            preserved_refs.iter().map(|(_, sha)| sha.clone()).collect();
        include_shas.sort();
        include_shas.dedup();

        if include_shas.is_empty() {
            return Ok(true);
        }

        let existing = self.git_batch_check_objects(include_shas.iter().map(|sha| sha.as_str()))?;
        if existing.len() != include_shas.len() {
            let missing: Vec<String> = include_shas
                .iter()
                .filter(|sha| !existing.contains(*sha))
                .cloned()
                .collect();
            self.detail(&format!(
                "  Local git is missing {} preserved remote tip(s): {}",
                missing.len(),
                missing
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return Ok(false);
        }

        let exclude_shas = self.resolved_push_tip_shas();
        let objects = match self.list_objects_for_shas(&include_shas, &exclude_shas) {
            Ok(objects) => objects,
            Err(err) => {
                self.detail(&format!(
                    "  Could not enumerate preserved remote objects from local git: {}",
                    err
                ));
                return Ok(false);
            }
        };

        self.detail(&format!(
            "  Importing {} preserved object(s) from local git for {} untouched ref(s)",
            objects.len(),
            preserved_refs.len()
        ));

        let objects_with_content = match self.read_git_objects_batch(&objects) {
            Ok(objects_with_content) => objects_with_content,
            Err(err) => {
                self.detail(&format!(
                    "  Could not read preserved remote objects from local git: {}",
                    err
                ));
                return Ok(false);
            }
        };

        for (obj_type, content) in objects_with_content {
            self.storage.write_raw_object(obj_type, &content)?;
        }

        Ok(true)
    }

    pub(super) fn resolved_push_tip_shas(&self) -> Vec<String> {
        let mut shas = Vec::new();
        for spec in &self.push_specs {
            if spec.src.is_empty() {
                continue;
            }
            if let Ok(sha) = self.resolve_ref(&spec.src) {
                shas.push(sha);
            }
        }
        shas.sort();
        shas.dedup();
        shas
    }

    /// Resolve a ref to its sha
    pub(super) fn resolve_ref(&self, refspec: &str) -> Result<String> {
        let output = Command::new("git").args(["rev-parse", refspec]).output()?;

        if !output.status.success() {
            bail!("Failed to resolve ref: {}", refspec);
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn resolve_tree_sha(sha: &str) -> Result<String> {
        let treeish = format!("{sha}^{{tree}}");
        let output = Command::new("git")
            .args(["rev-parse", "--verify", &treeish])
            .output()
            .context("run git rev-parse for base tree")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "Failed to resolve tree for {}{}",
                sha,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }

        let tree_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !Self::is_hex_object_id(&tree_sha) {
            bail!("Resolved invalid tree id for {}", sha);
        }
        Ok(tree_sha)
    }

    /// Check if ancestor_sha is an ancestor of descendant_sha
    pub(super) fn check_ancestor(&self, ancestor_sha: &str, descendant_sha: &str) -> AncestorCheck {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor_sha, descendant_sha])
            .output();

        match output {
            Ok(o) => Self::classify_merge_base_result(o.status.code(), &o.stderr),
            Err(e) => AncestorCheck::Unknown(format!("failed to run git merge-base: {}", e)),
        }
    }

    pub(super) fn classify_merge_base_result(
        status_code: Option<i32>,
        stderr: &[u8],
    ) -> AncestorCheck {
        match status_code {
            Some(0) => AncestorCheck::Ancestor,
            Some(1) => AncestorCheck::NotAncestor,
            Some(code) => {
                let stderr = String::from_utf8_lossy(stderr).trim().to_string();
                if stderr.is_empty() {
                    AncestorCheck::Unknown(format!("git merge-base exited with exit code {}", code))
                } else {
                    AncestorCheck::Unknown(format!(
                        "git merge-base exited with exit code {}: {}",
                        code, stderr
                    ))
                }
            }
            None => {
                let stderr = String::from_utf8_lossy(stderr).trim().to_string();
                if stderr.is_empty() {
                    AncestorCheck::Unknown(
                        "git merge-base terminated with no exit code".to_string(),
                    )
                } else {
                    AncestorCheck::Unknown(format!(
                        "git merge-base terminated with no exit code: {}",
                        stderr
                    ))
                }
            }
        }
    }

    fn repo_announcement_options_for_public_push(
        &self,
        sha: &str,
    ) -> Option<RepoAnnouncementOptions> {
        if self.is_private || self.url_secret.is_some() {
            return None;
        }

        let local_root_commit = match self.git_root_commit_for_tip(sha) {
            Ok(root_commit) => root_commit,
            Err(err) => {
                warn!("Could not determine root commit for NIP-34 euc: {}", err);
                None
            }
        };

        let fork_source = self.infer_htree_fork_source_repo();
        let source_euc = fork_source.as_ref().and_then(|source| {
            match self
                .nostr
                .fetch_repo_announcement_euc(&source.pubkey_hex, &source.repo_name)
            {
                Ok(euc) => euc,
                Err(err) => {
                    debug!(
                        "Could not fetch source NIP-34 euc for {}: {}",
                        source.canonical_url, err
                    );
                    None
                }
            }
        });

        let earliest_unique_commit = source_euc.or(local_root_commit);
        let (personal_fork, forked_from) = match fork_source {
            Some(source) => (true, Some(source.canonical_url)),
            None => (false, None),
        };

        Some(RepoAnnouncementOptions {
            earliest_unique_commit,
            personal_fork,
            forked_from,
        })
    }

    fn git_root_commit_for_tip(&self, sha: &str) -> Result<Option<String>> {
        let output = Command::new("git")
            .args(["rev-list", "--max-parents=0", "--reverse", sha])
            .output()
            .context("run git rev-list for NIP-34 euc")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "git rev-list failed{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| Self::is_hex_object_id(line))
            .map(ToString::to_string))
    }

    fn infer_htree_fork_source_repo(&self) -> Option<ForkSourceRepo> {
        let output = Command::new("git")
            .args(["config", "--get-regexp", r"^remote\..*\.url$"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let mut candidates = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some(split_at) = line.find(char::is_whitespace) else {
                continue;
            };
            let key = &line[..split_at];
            let url = line[split_at..].trim();
            let remote_name = key
                .strip_prefix("remote.")
                .and_then(|value| value.strip_suffix(".url"))
                .unwrap_or_default();
            let Some((identifier, repo_name)) = Self::parse_htree_remote_url(url) else {
                continue;
            };
            let Ok((source_pubkey_hex, _)) = resolve_identity(&identifier) else {
                continue;
            };
            if source_pubkey_hex.eq_ignore_ascii_case(&self.pubkey) {
                continue;
            }

            let source_npub = Self::pubkey_hex_to_npub(&source_pubkey_hex)
                .unwrap_or_else(|| source_pubkey_hex.clone());
            let canonical_url = format!("htree://{}/{}", source_npub, repo_name);
            let priority = match remote_name {
                "origin" => 0,
                "upstream" => 1,
                _ => 2,
            };

            candidates.push((
                priority,
                canonical_url.clone(),
                ForkSourceRepo {
                    pubkey_hex: source_pubkey_hex,
                    repo_name,
                    canonical_url,
                },
            ));
        }

        candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        candidates.into_iter().map(|(_, _, source)| source).next()
    }

    fn parse_htree_remote_url(url: &str) -> Option<(String, String)> {
        let raw = url.strip_prefix("htree://")?;
        let path = raw.split('#').next().unwrap_or(raw);
        let (identifier, repo_name) = path.split_once('/')?;
        if identifier.is_empty() || repo_name.is_empty() {
            return None;
        }
        Some((identifier.to_string(), repo_name.to_string()))
    }

    fn pubkey_hex_to_npub(pubkey_hex: &str) -> Option<String> {
        PublicKey::from_hex(pubkey_hex)
            .ok()
            .and_then(|pubkey| pubkey.to_bech32().ok())
    }

    /// Push all objects reachable from sha
    pub(super) fn push_objects(
        &mut self,
        sha: &str,
        dst_ref: &str,
        force_push: bool,
        remote_tip_sha: Option<&str>,
    ) -> Result<()> {
        if !force_push && remote_tip_sha == Some(sha) {
            debug!(
                "Skipping push for {} because remote tip already equals {}",
                dst_ref, sha
            );
            return Ok(());
        }

        eprintln!("  Listing objects...");
        let delta_base = self.delta_base_for_push(sha, force_push, remote_tip_sha);
        let objects = self.list_objects_for_push(sha, delta_base.as_deref())?;
        if let Some(base) = delta_base.as_deref() {
            eprintln!(
                "  Listed {} object(s) (delta from {})",
                objects.len(),
                &base[..12.min(base.len())]
            );
        } else {
            eprintln!("  Listed {} object(s)", objects.len());
        }

        info!("Pushing {} objects for {}", objects.len(), sha);

        let oid = crate::git::object::ObjectId::from_hex(sha)
            .ok_or_else(|| anyhow::anyhow!("Invalid object id: {}", sha))?;
        self.storage.write_ref(dst_ref, &Ref::Direct(oid))?;

        if dst_ref.starts_with("refs/heads/") {
            self.storage
                .write_ref("HEAD", &Ref::Symbolic(dst_ref.to_string()))?;
            debug!("Set HEAD -> {}", dst_ref);
        }

        let base_has_pack_checkpoint = self.cached_remote_root_pack_checkpoint_available();
        let rebuild_checkpoint_from_first_bucket =
            delta_base.is_some() && !base_has_pack_checkpoint;
        let checkpoint_covered = match self.prepare_git_pack_checkpoint(
            sha,
            objects.len(),
            delta_base.as_deref(),
            rebuild_checkpoint_from_first_bucket,
        ) {
            Ok(Some(covered)) => covered,
            Ok(None) => HashSet::new(),
            Err(err) => {
                warn!("Git pack checkpoint skipped: {}", err);
                if self.is_slow() {
                    eprintln!("  Warning: git pack checkpoint skipped: {}", err);
                }
                HashSet::new()
            }
        };

        let objects_to_import = self.select_objects_to_import_for_push(
            sha,
            &objects,
            &checkpoint_covered,
            base_has_pack_checkpoint,
        )?;
        if checkpoint_covered.is_empty() && base_has_pack_checkpoint {
            let inherited_pack_covered =
                self.inherited_pack_covered_imported_tree_ids(sha, &objects)?;
            self.storage
                .set_pack_checkpoint_files(BTreeMap::new(), inherited_pack_covered)?;
        }
        if checkpoint_covered.is_empty() {
            eprintln!("  Reading objects...");
        } else {
            eprintln!(
                "  Reading needed objects... {}/{} object(s)",
                objects_to_import.len(),
                objects.len()
            );
        }
        let objects_with_content = self.read_git_objects_batch(&objects_to_import)?;

        eprintln!("  Writing to local store...");
        Self::write_objects_to_local_store(&self.storage, objects_with_content)?;

        if !self.nostr.can_sign() {
            anyhow::bail!(
                "Cannot push: no secret key for {}. You can only push to your own repos.",
                self.nostr.npub()
            );
        }

        let mut root_cid = if let Some(base) = delta_base.as_deref() {
            match self.build_tree_with_cached_remote_root(
                "Merging delta with cached remote root",
                Some(base),
            ) {
                Ok(Some(root_cid)) => root_cid,
                Ok(None) => match self.build_tree_with_progress("Building repo tree") {
                    Ok(root_cid) => root_cid,
                    Err(err) => self.repair_delta_tree_build(
                        sha,
                        dst_ref,
                        base,
                        "Cached remote root unavailable",
                        err.to_string(),
                    )?,
                },
                Err(err) => self.repair_delta_tree_build(
                    sha,
                    dst_ref,
                    base,
                    "Cached-root merge incomplete",
                    err.to_string(),
                )?,
            }
        } else {
            self.build_tree_with_progress("Building repo tree")?
        };
        if let Err(validation_err) = self.storage.validate_root_contains_direct_refs(&root_cid) {
            eprintln!(
                "  Built repo tree is missing ref object(s) ({}); rebuilding from full local import",
                validation_err
            );
            self.import_full_local_revision(sha)?;
            root_cid =
                self.build_tree_with_progress("Rebuilding repo tree after full local import")?;
            self.storage
                .validate_root_contains_direct_refs(&root_cid)
                .context("rebuilt repo tree is still missing ref objects")?;
        }
        let root_hash_hex = hex::encode(root_cid.hash);
        let chk_key = root_cid.key;
        let is_link_visible = self.url_secret.is_some();
        if self.is_slow() {
            eprintln!(
                " done (encrypted: {}, link_visible: {}, private: {})",
                chk_key.is_some(),
                is_link_visible,
                self.is_private
            );
        }

        let key_to_publish = if let (Some(chk), Some(secret)) = (chk_key, self.url_secret) {
            let mut masked = [0u8; 32];
            for i in 0..32 {
                masked[i] = chk[i] ^ secret[i];
            }
            Some(masked)
        } else {
            chk_key
        };

        let old_root_hash = self.nostr.get_cached_root_hash(&self.repo_name).cloned();
        let old_encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .copied();
        let blossom_result = self.push_to_file_servers_with_diff(
            &root_hash_hex,
            chk_key.as_ref(),
            old_root_hash.as_deref(),
            old_encryption_key.as_ref(),
            true,
        );
        ensure_blossom_publish_ready(&blossom_result)?;
        self.verify_root_available_on_write_server(&root_hash_hex)?;

        let key_with_privacy = key_to_publish
            .as_ref()
            .map(|k| (k, is_link_visible, self.is_private));
        let repo_announcement = self.repo_announcement_options_for_public_push(sha);
        let (npub_url, relay_result) = self
            .nostr
            .publish_repo_with_announcement(
                &self.repo_name,
                &root_hash_hex,
                key_with_privacy,
                repo_announcement,
            )
            .map_err(|e| anyhow::anyhow!("Failed to publish repo metadata to relays: {}", e))?;

        let full_url = if let Some(secret) = self.url_secret {
            format!("{}#k={}", npub_url, hex::encode(secret))
        } else {
            npub_url.clone()
        };

        eprintln!("Published to: {}", full_url);
        if !relay_result.connected.is_empty() {
            eprintln!("  Relays: {}", relay_result.connected.join(", "));
        } else {
            eprintln!("  Relays: none");
        }
        if !relay_result.failed.is_empty() {
            eprintln!("  Relays failed: {}", relay_result.failed.join(", "));
        }
        if !blossom_result.succeeded.is_empty() {
            eprintln!("  Blossom: {}", blossom_result.succeeded.join(", "));
        }
        if !blossom_result.failed.is_empty() {
            eprintln!("  Blossom failed: {}", blossom_result.failed.join(", "));
        }
        eprintln!("  Config: ~/.hashtree/config.toml");

        if let Some(path) = npub_url.strip_prefix("htree://") {
            let viewer_url = build_repo_viewer_url(path, self.url_secret.as_ref());
            eprintln!("View at: {}", viewer_url);
        }

        match self.storage.evict_if_needed() {
            Ok(freed) if freed > 0 => {
                info!(
                    "Evicted {} bytes from shared git blob cache after push",
                    freed
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to evict shared git blob cache after push: {}", err);
            }
        }

        Ok(())
    }

    fn list_objects_for_push(&self, sha: &str, delta_base: Option<&str>) -> Result<Vec<String>> {
        let exclude: Vec<String> = delta_base
            .map(|base| vec![base.to_string()])
            .unwrap_or_default();
        self.list_objects_to_push(sha, &exclude)
    }

    fn write_objects_to_local_store(
        storage: &crate::git::storage::GitStorage,
        objects_with_content: Vec<(crate::git::object::ObjectType, Vec<u8>)>,
    ) -> Result<()> {
        let total = objects_with_content.len();
        for (i, (obj_type, content)) in objects_with_content.into_iter().enumerate() {
            storage.write_raw_object(obj_type, &content)?;
            if (i + 1) % 1000 == 0 || i + 1 == total {
                eprintln!("  Writing to local store: {}/{}", i + 1, total);
            }
        }
        Ok(())
    }

    fn prepare_git_pack_checkpoint(
        &self,
        sha: &str,
        object_count: usize,
        delta_base: Option<&str>,
        rebuild_checkpoint_from_first_bucket: bool,
    ) -> Result<Option<HashSet<String>>> {
        self.storage
            .set_pack_checkpoint_files(BTreeMap::new(), HashSet::new())?;
        let min_objects = git_pack_checkpoint_min_objects();
        if min_objects == 0 {
            return Ok(None);
        }

        let Some(plan) = Self::plan_git_pack_checkpoint(
            sha,
            object_count,
            delta_base,
            min_objects,
            rebuild_checkpoint_from_first_bucket,
        )?
        else {
            return Ok(None);
        };

        if self.is_slow() {
            eprintln!("  Building git pack checkpoint...");
        }
        let mut pack_files = BTreeMap::new();
        for pack in &plan.packs {
            let generated =
                Self::generate_git_pack_checkpoint(&pack.tip, pack.exclude_tip.as_deref())?;
            pack_files.extend(generated);
        }
        let total_bytes: usize = pack_files.values().map(Vec::len).sum();
        let file_count = pack_files.len();
        let covered_objects = plan.covered_objects;
        let returned_covered_objects = covered_objects.clone();
        self.storage
            .set_pack_checkpoint_files(pack_files, covered_objects)?;
        if self.is_slow() {
            eprintln!(" {} files, {} bytes", file_count, total_bytes);
        }
        Ok(Some(returned_covered_objects))
    }

    pub(super) fn plan_git_pack_checkpoint(
        sha: &str,
        object_count: usize,
        delta_base: Option<&str>,
        interval_objects: usize,
        rebuild_checkpoint_from_first_bucket: bool,
    ) -> Result<Option<GitPackCheckpointPlan>> {
        let total_objects = if delta_base.is_none() && !rebuild_checkpoint_from_first_bucket {
            object_count
        } else {
            Self::reachable_git_object_count(sha)?
        };
        if total_objects < interval_objects {
            return Ok(None);
        }

        let bucket = total_objects / interval_objects;
        let mut first_bucket = 1;
        let mut previous_tip = None;
        if let Some(base) = delta_base.filter(|_| !rebuild_checkpoint_from_first_bucket) {
            let base_objects = Self::reachable_git_object_count(base).unwrap_or(0);
            let base_bucket = base_objects / interval_objects;
            if bucket <= base_bucket {
                return Ok(None);
            }
            if base_bucket > 0 {
                previous_tip = Some(Self::find_git_pack_checkpoint_tip(
                    base,
                    base_bucket * interval_objects,
                )?);
            }
            first_bucket = base_bucket + 1;
        }

        let mut packs = Vec::new();
        for checkpoint_bucket in first_bucket..=bucket {
            let tip =
                Self::find_git_pack_checkpoint_tip(sha, checkpoint_bucket * interval_objects)?;
            if previous_tip.as_deref() == Some(tip.as_str()) {
                continue;
            }
            packs.push(GitPackCheckpointPackPlan {
                tip: tip.clone(),
                exclude_tip: previous_tip.clone(),
            });
            previous_tip = Some(tip);
        }

        if packs.is_empty() {
            return Ok(None);
        }

        let Some(covered_tip) = previous_tip else {
            return Ok(None);
        };
        let covered_objects = Self::reachable_git_object_ids(&covered_tip)?;
        Ok(Some(GitPackCheckpointPlan {
            packs,
            covered_objects,
        }))
    }

    pub(super) fn reachable_git_object_count(sha: &str) -> Result<usize> {
        Ok(Self::reachable_git_object_ids(sha)?.len())
    }

    fn reachable_git_object_ids(sha: &str) -> Result<HashSet<String>> {
        let output = Command::new("git")
            .args(["rev-list", "--objects", "--no-object-names", sha])
            .output()
            .context("run git rev-list for checkpoint interval")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "git rev-list failed while checking checkpoint interval{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| line.len() == 40 && line.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_string)
            .collect())
    }

    fn rev_list_first_parent_commits(sha: &str) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(["rev-list", "--first-parent", "--reverse", sha])
            .output()
            .context("run git rev-list for checkpoint commits")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "git rev-list failed while choosing checkpoint commit{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| line.len() == 40 && line.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_string)
            .collect())
    }

    fn find_git_pack_checkpoint_tip(sha: &str, target_objects: usize) -> Result<String> {
        let commits = Self::rev_list_first_parent_commits(sha)?;
        if commits.is_empty() {
            return Ok(sha.to_string());
        }

        let first_count = Self::reachable_git_object_count(&commits[0])?;
        if first_count >= target_objects {
            return Ok(commits[0].clone());
        }

        let mut lo = 0usize;
        let mut hi = commits.len() - 1;
        let mut best = 0usize;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let count = Self::reachable_git_object_count(&commits[mid])?;
            if count <= target_objects {
                best = mid;
                lo = mid.saturating_add(1);
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }

        Ok(commits[best].clone())
    }

    pub(super) fn generate_git_pack_checkpoint(
        tip: &str,
        exclude_tip: Option<&str>,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let temp_dir = unique_git_pack_temp_dir();
        std::fs::create_dir_all(&temp_dir)
            .with_context(|| format!("create {}", temp_dir.display()))?;
        let pack_prefix = temp_dir.join("pack");
        let pack_prefix_str = pack_prefix
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("temporary pack path is not valid UTF-8"))?;

        let mut child = Command::new("git")
            .args([
                "pack-objects",
                "--threads=1",
                "--window=10",
                "--depth=50",
                "--compression=6",
                "--no-reuse-delta",
                "--no-reuse-object",
                "--no-use-bitmap-index",
                "--revs",
                pack_prefix_str,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn git pack-objects")?;

        {
            let stdin = child
                .stdin
                .as_mut()
                .context("open git pack-objects stdin")?;
            writeln!(stdin, "{}", tip)?;
            if let Some(exclude_tip) = exclude_tip {
                writeln!(stdin, "^{}", exclude_tip)?;
            }
        }

        let output = child
            .wait_with_output()
            .context("wait for git pack-objects")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let _ = std::fs::remove_dir_all(&temp_dir);
            bail!(
                "git pack-objects failed{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }

        let pack_hash = String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .map(str::trim)
            .filter(|line| line.len() == 40 && line.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| anyhow::anyhow!("git pack-objects did not print a pack hash"))?
            .to_string();

        let pack_name = format!("pack-{}.pack", pack_hash);
        let idx_name = format!("pack-{}.idx", pack_hash);
        let pack_path = temp_dir.join(&pack_name);
        let idx_path = temp_dir.join(&idx_name);

        let mut files = BTreeMap::new();
        files.insert(
            pack_name,
            std::fs::read(&pack_path).with_context(|| format!("read {}", pack_path.display()))?,
        );
        files.insert(
            idx_name,
            std::fs::read(&idx_path).with_context(|| format!("read {}", idx_path.display()))?,
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(files)
    }

    fn is_hex_object_id(value: &str) -> bool {
        value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn current_tree_object_ids_for_push(sha: &str, include_blobs: bool) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        if include_blobs && Self::is_hex_object_id(sha) {
            ids.insert(sha.to_string());
        }

        let treeish = format!("{sha}^{{tree}}");
        let output = Command::new("git")
            .args(["rev-parse", "--verify", &treeish])
            .output()
            .context("run git rev-parse for current tree")?;
        if !output.status.success() {
            return Ok(ids);
        }

        let root_tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !Self::is_hex_object_id(&root_tree) {
            return Ok(ids);
        }
        ids.insert(root_tree.clone());

        let output = Command::new("git")
            .args(["ls-tree", "-r", "-t", "--full-tree", &root_tree])
            .output()
            .context("run git ls-tree for current tree")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "git ls-tree failed while selecting current tree objects{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }

        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.split_whitespace();
            let _mode = parts.next();
            let object_type = parts.next();
            let oid = parts.next();
            match (object_type, oid) {
                (Some("tree"), Some(oid)) if Self::is_hex_object_id(oid) => {
                    ids.insert(oid.to_string());
                }
                (Some("blob"), Some(oid)) if include_blobs && Self::is_hex_object_id(oid) => {
                    ids.insert(oid.to_string());
                }
                _ => {}
            }
        }

        Ok(ids)
    }

    pub(super) fn current_tree_object_ids(sha: &str) -> Result<HashSet<String>> {
        Self::current_tree_object_ids_for_push(sha, true)
    }

    pub(super) fn current_tree_tree_object_ids(sha: &str) -> Result<HashSet<String>> {
        Self::current_tree_object_ids_for_push(sha, false)
    }

    pub(super) fn inherited_pack_covered_imported_tree_candidates(
        sha: &str,
        listed_objects: &[String],
    ) -> Result<HashSet<String>> {
        let listed: HashSet<&str> = listed_objects.iter().map(String::as_str).collect();
        Ok(Self::current_tree_tree_object_ids(sha)?
            .into_iter()
            .filter(|oid| !listed.contains(oid.as_str()))
            .collect())
    }

    fn inherited_pack_covered_imported_tree_ids(
        &self,
        sha: &str,
        listed_objects: &[String],
    ) -> Result<HashSet<String>> {
        let candidates =
            Self::inherited_pack_covered_imported_tree_candidates(sha, listed_objects)?;
        if candidates.is_empty() {
            return Ok(candidates);
        }

        let Some(root_hash) = self.nostr.get_cached_root_hash(&self.repo_name).cloned() else {
            return Ok(HashSet::new());
        };
        let encryption_key = self
            .nostr
            .get_cached_encryption_key(&self.repo_name)
            .copied();
        let (cached_tree, _) = self.build_cached_fetch_tree()?;
        let root_bytes = hex::decode(&root_hash).context("Invalid cached root hash hex")?;
        let root_arr: [u8; 32] = root_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cached root hash must be 32 bytes"))?;
        let cached_root_cid = hashtree_core::Cid {
            hash: root_arr,
            key: encryption_key,
        };

        block_on_result(async {
            let Some(objects_cid) = cached_tree
                .resolve_path(&cached_root_cid, ".git/objects")
                .await
                .context("resolve cached .git/objects")?
            else {
                return Ok(HashSet::new());
            };

            let mut pack_covered = HashSet::new();
            for oid in candidates {
                let loose_path = format!("{}/{}", &oid[..2], &oid[2..]);
                let loose_exists = cached_tree
                    .resolve_path(&objects_cid, &loose_path)
                    .await
                    .with_context(|| format!("resolve cached loose object {oid}"))?
                    .is_some();
                if !loose_exists {
                    pack_covered.insert(oid);
                }
            }

            Ok(pack_covered)
        })
    }

    pub(super) fn select_objects_to_import_for_push(
        &self,
        sha: &str,
        listed_objects: &[String],
        checkpoint_covered: &HashSet<String>,
        base_has_pack_checkpoint: bool,
    ) -> Result<Vec<String>> {
        let mut selected = HashSet::new();
        for oid in listed_objects {
            if checkpoint_covered.is_empty() || !checkpoint_covered.contains(oid) {
                selected.insert(oid.clone());
            }
        }

        if !checkpoint_covered.is_empty() {
            selected.extend(Self::current_tree_object_ids(sha)?);
        } else if base_has_pack_checkpoint {
            selected.extend(Self::current_tree_tree_object_ids(sha)?);
        }

        let mut selected: Vec<String> = selected.into_iter().collect();
        selected.sort();
        Ok(selected)
    }

    fn import_full_local_revision(&mut self, sha: &str) -> Result<()> {
        eprintln!("  Listing objects...");
        let objects = self.list_objects_to_push(sha, &[])?;
        eprintln!("  Listed {} object(s) (full rebuild)", objects.len());

        let objects_with_content = self.read_git_objects_batch(&objects)?;

        eprintln!("  Writing to local store...");
        Self::write_objects_to_local_store(&self.storage, objects_with_content)?;

        Ok(())
    }

    /// Find merged-in parent SHAs from merge commits in a pushed range.
    pub(super) fn find_merged_parent_shas(&self, range: &str) -> Result<HashSet<String>> {
        let output = Command::new("git")
            .args(["rev-list", "--merges", "--parents", range])
            .output()
            .context("Failed to run git rev-list")?;

        if !output.status.success() {
            return Ok(HashSet::new());
        }

        let merged_parent_shas: HashSet<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .flat_map(|line| line.split_whitespace().skip(2).map(str::to_owned))
            .collect();

        Ok(merged_parent_shas)
    }

    /// Detect merged PRs in pushed refs and publish status events
    pub(super) fn detect_and_mark_merged_prs(&self, pushed_refs: &[(String, String)]) {
        let open_prs = match self
            .nostr
            .fetch_prs(&self.repo_name, PullRequestStateFilter::Open)
        {
            Ok(prs) => prs,
            Err(e) => {
                debug!("Failed to fetch open PRs: {}", e);
                return;
            }
        };

        if open_prs.is_empty() {
            return;
        }

        let merge_candidates = pushed_refs
            .iter()
            .filter_map(|(dst_ref, sha)| {
                dst_ref
                    .strip_prefix("refs/heads/")
                    .map(|branch_name| (dst_ref, branch_name, sha))
            })
            .filter_map(|(dst_ref, branch_name, sha)| {
                let Some(old_sha) = self.remote_refs.get(dst_ref) else {
                    debug!(
                        "Skipping PR auto-merge detection for {}: previous remote tip is unknown",
                        dst_ref
                    );
                    return None;
                };

                let range = format!("{}..{}", old_sha, sha);
                let merged_parent_shas = match self.find_merged_parent_shas(&range) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("Failed to find merge commits for {}: {}", dst_ref, e);
                        return None;
                    }
                };

                if merged_parent_shas.is_empty() {
                    return None;
                }

                debug!(
                    "Found {} merged parent SHAs in push to {}",
                    merged_parent_shas.len(),
                    dst_ref
                );

                Some((branch_name, merged_parent_shas))
            });

        for (branch_name, merged_parent_shas) in merge_candidates {
            let matching_prs = open_prs
                .iter()
                .filter(|pr| pr.target_branch.as_deref().unwrap_or("master") == branch_name)
                .filter(|pr| {
                    pr.commit_tip
                        .as_ref()
                        .is_some_and(|commit_tip| merged_parent_shas.contains(commit_tip))
                });

            for pr in matching_prs {
                match self
                    .nostr
                    .publish_pr_merged_status(&pr.event_id, &pr.author_pubkey)
                {
                    Ok(()) => {
                        eprintln!(
                            "PR auto-merged: ({})...",
                            &pr.event_id[..12.min(pr.event_id.len())]
                        );
                    }
                    Err(e) => {
                        debug!("Failed to publish PR merged status: {}", e);
                    }
                }
            }
        }
    }

    /// Push content to file servers (blossom) with efficient diff-based upload
    pub(super) fn push_to_file_servers_with_diff(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
        old_root_hash: Option<&str>,
        old_encryption_key: Option<&[u8; 32]>,
        trust_server_old_tree_coverage: bool,
    ) -> BlossomResult {
        use hashtree_core::crypto::decrypt_chk;
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let blossom = self.nostr.blossom();
        let configured: Vec<String> = blossom.write_servers().to_vec();

        let rt = match new_multi_thread_runtime() {
            Ok(rt) => rt,
            Err(e) => {
                warn!("Failed to create runtime for blossom upload: {}", e);
                return BlossomResult {
                    configured: configured.clone(),
                    succeeded: vec![],
                    failed: configured,
                    local_complete: false,
                    degraded: true,
                };
            }
        };

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
                    local_complete: false,
                    degraded: true,
                };
            }
        };

        let force_upload = self.config.blossom.force_upload;
        let old_root_bytes: Option<[u8; 32]> = if force_upload {
            None
        } else {
            old_root_hash.and_then(|h| {
                hex::decode(h).ok().and_then(|b| {
                    if b.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&b);
                        Some(arr)
                    } else {
                        None
                    }
                })
            })
        };

        let verbose = self.is_slow();
        let trust_server_old_tree_coverage = trust_server_old_tree_coverage && !force_upload;
        let (local_complete, degraded_replication) = rt.block_on(async {
            use hashtree_core::{Cid, HashTree, HashTreeConfig};
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Arc;
            use tokio::sync::mpsc;

            let uploaded = Arc::new(AtomicUsize::new(0));
            let skipped_diff = Arc::new(AtomicUsize::new(0));
            let skipped_server = Arc::new(AtomicUsize::new(0));
            let failed = Arc::new(AtomicUsize::new(0));
            let local_failed = Arc::new(AtomicUsize::new(0));
            let completed = Arc::new(AtomicUsize::new(0));
            let discovered_total = Arc::new(AtomicUsize::new(1));
            let discovery_complete = Arc::new(AtomicBool::new(false));
            let counters = UploadCounters {
                uploaded: Arc::clone(&uploaded),
                skipped_diff: Arc::clone(&skipped_diff),
                skipped_server: Arc::clone(&skipped_server),
                failed: Arc::clone(&failed),
                completed: Arc::clone(&completed),
                discovered_total: Arc::clone(&discovered_total),
            };

            let old_hashes: HashSet<[u8; 32]> = if let Some(old_root) = old_root_bytes {
                if old_root == root_bytes {
                    if verbose {
                        eprintln!("  No changes detected (same root hash)");
                    }
                    return (true, false);
                }

                if verbose {
                    eprintln!("  Computing diff from previous tree...");
                }

                let tree = HashTree::new(HashTreeConfig::new(store.clone()));
                let old_cid = Cid {
                    hash: old_root,
                    key: old_encryption_key.copied(),
                };

                match collect_complete_hashes(&tree, &old_cid, 32).await {
                    Ok(hashes) => {
                        if verbose {
                            eprintln!(" {} hashes in old tree", hashes.len());
                        }
                        hashes
                    }
                    Err(local_err) => {
                        match self.build_cached_fetch_tree() {
                            Ok((cached_tree, _)) => match collect_complete_hashes(&cached_tree, &old_cid, 32).await {
                                Ok(hashes) => {
                                    if verbose {
                                        eprintln!(
                                            " {} hashes in old tree (via cached fetch tree after local miss: {})",
                                            hashes.len(),
                                            local_err
                                        );
                                    }
                                    hashes
                                }
                                Err(cached_err) => {
                                    if verbose {
                                        eprintln!(" failed locally: {}", local_err);
                                        eprintln!("  Cached old-tree walk failed too: {}", cached_err);
                                        eprintln!("  Falling back to full upload");
                                    }
                                    HashSet::new()
                                }
                            },
                            Err(build_err) => {
                                if verbose {
                                    eprintln!(" failed locally: {}", local_err);
                                    eprintln!("  Could not build cached fetch tree: {}", build_err);
                                    eprintln!("  Falling back to full upload");
                                }
                                HashSet::new()
                            }
                        }
                    }
                }
            } else {
                HashSet::new()
            };

            let has_old_tree = !old_hashes.is_empty();
            let old_tree_unavailable = old_root_bytes.is_some() && !has_old_tree;
            let all_servers: Vec<String> = blossom.write_servers().to_vec();
            let servers_needing_full: Arc<Vec<String>> =
                if has_old_tree && all_servers.len() == 1 {
                    let old_root = old_root_bytes.unwrap();
                    let sample_hashes: Vec<String> = old_hashes
                        .iter()
                        .filter(|h| **h != old_root)
                        .take(SERVER_COVERAGE_SAMPLE_SIZE)
                        .map(hex::encode)
                        .collect();
                    if sample_hashes.is_empty() {
                        Arc::new(Vec::new())
                    } else {
                        let sample_refs: Vec<&str> =
                            sample_hashes.iter().map(|s| s.as_str()).collect();
                        match blossom
                            .server_tree_sample_coverage(
                                &all_servers[0],
                                &sample_refs,
                                SERVER_COVERAGE_SAMPLE_SIZE,
                            )
                            .await
                        {
                            hashtree_blossom::BlobAvailability::Missing => {
                                if verbose {
                                    let server_name = all_servers[0]
                                        .trim_start_matches("https://")
                                        .trim_start_matches("http://")
                                        .split('/')
                                        .next()
                                        .unwrap_or(&all_servers[0]);
                                    eprintln!(
                                        "  Full upload needed: {} (missing old tree)",
                                        server_name
                                    );
                                }
                                Arc::new(all_servers.clone())
                            }
                            hashtree_blossom::BlobAvailability::Unknown => {
                                if verbose {
                                    let server_name = all_servers[0]
                                        .trim_start_matches("https://")
                                        .trim_start_matches("http://")
                                        .split('/')
                                        .next()
                                        .unwrap_or(&all_servers[0]);
                                    eprintln!(
                                        "  Old-tree coverage probe inconclusive: {}",
                                        server_name
                                    );
                                }
                                Arc::new(Vec::new())
                            }
                            hashtree_blossom::BlobAvailability::Present => Arc::new(Vec::new()),
                        }
                    }
                } else {
                    Arc::new(Vec::new())
                };
            let prune_known_subtrees =
                has_old_tree && trust_server_old_tree_coverage && servers_needing_full.is_empty();
            let use_upload_check =
                !force_upload && (old_tree_unavailable || !prune_known_subtrees);
            if verbose && use_upload_check {
                eprintln!("  Checking server blob inventory in upload batches");
            }

            const CHANNEL_SIZE: usize = 100;
            let upload_concurrency = self.upload_concurrency(all_servers.len());
            let (tx, rx) = mpsc::channel::<([u8; 32], Vec<u8>, bool, bool, bool)>(CHANNEL_SIZE);

            let upload_handle = {
                let blossom = blossom.clone();
                let uploaded = Arc::clone(&uploaded);
                let skipped_server = Arc::clone(&skipped_server);
                let failed = Arc::clone(&failed);
                let completed = Arc::clone(&completed);
                let servers_needing_full = Arc::clone(&servers_needing_full);

                tokio::spawn(async move {
                    use futures::stream::StreamExt;
                    use tokio_stream::wrappers::ReceiverStream;

                    let stream = ReceiverStream::new(rx);
                    stream
                        .map(|(hash, data, from_old_tree, force_all_servers, head_fallback)| {
                            let blossom = blossom.clone();
                            let uploaded = Arc::clone(&uploaded);
                            let skipped_server = Arc::clone(&skipped_server);
                            let failed = Arc::clone(&failed);
                            let completed = Arc::clone(&completed);
                            let servers_needing_full = Arc::clone(&servers_needing_full);
                            async move {
                                let result = if head_fallback {
                                    let hash_hex = hex::encode(hash);
                                    if blossom.exists(&hash_hex).await {
                                        Ok((hash_hex, false))
                                    } else {
                                        upload_block_to_file_servers(
                                            &blossom,
                                            &data,
                                            from_old_tree,
                                            force_all_servers,
                                            servers_needing_full.as_ref().as_slice(),
                                        )
                                        .await
                                    }
                                } else {
                                    upload_block_to_file_servers(
                                        &blossom,
                                        &data,
                                        from_old_tree,
                                        force_all_servers,
                                        servers_needing_full.as_ref().as_slice(),
                                    )
                                    .await
                                };
                                match result {
                                    Ok((_, true)) => {
                                        uploaded.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Ok((_, false)) => {
                                        skipped_server.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        failed.fetch_add(1, Ordering::Relaxed);
                                        eprintln!("\n  Upload failed ({} bytes): {}", data.len(), e);
                                    }
                                }
                                completed.fetch_add(1, Ordering::Relaxed);
                            }
                        })
                        .buffer_unordered(upload_concurrency)
                        .for_each(|_| async {})
                        .await;
                })
            };

            let mut visited: HashSet<[u8; 32]> = HashSet::new();
            let mut queued: HashSet<[u8; 32]> = HashSet::new();
            let mut queue: Vec<([u8; 32], Option<[u8; 32]>)> = Vec::new();
            let mut pending_uploads = Vec::with_capacity(UPLOAD_CHECK_BATCH_SIZE.min(1024));
            let mut upload_check_supported = true;
            let _ = queue_hash_if_new(&mut queue, &mut queued, root_bytes, encryption_key.copied());

            let (progress_done_tx, progress_done_rx) = tokio::sync::watch::channel(false);
            let progress_handle = spawn_periodic_upload_progress(
                counters.clone(),
                Arc::clone(&discovery_complete),
                has_old_tree,
                progress_done_rx,
            );
            emit_upload_progress(upload_progress_from_counters(
                &counters,
                &discovery_complete,
                has_old_tree,
            ));

            while let Some((hash, key)) = queue.pop() {
                if visited.contains(&hash) {
                    continue;
                }
                visited.insert(hash);
                let from_old_tree = old_hashes.contains(&hash);

                let mut force_all_servers_for_hash = false;
                if from_old_tree {
                    if prune_known_subtrees {
                        skipped_diff.fetch_add(1, Ordering::Relaxed);
                        completed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    } else {
                        force_all_servers_for_hash = true;
                    }
                }

                let data = match store.get_sync(&hash) {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        local_failed.fetch_add(1, Ordering::Relaxed);
                        completed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("\n  Missing from local store: {}", hex::encode(hash));
                        continue;
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        local_failed.fetch_add(1, Ordering::Relaxed);
                        completed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("\n  Store read error for {}: {}", hex::encode(hash), e);
                        continue;
                    }
                };

                let plaintext = if let Some(k) = key {
                    match decrypt_chk(&data, &k) {
                        Ok(p) => p,
                        Err(_) => data.clone(),
                    }
                } else {
                    data.clone()
                };

                if let Some(node) = try_decode_tree_node(&plaintext) {
                    queue_links_for_diff_upload(
                        &mut queue,
                        &mut queued,
                        &node.links,
                        &old_hashes,
                        prune_known_subtrees,
                        &discovered_total,
                    );
                }

                pending_uploads.push(PendingUpload {
                    hash,
                    data,
                    from_old_tree,
                    force_all_servers: force_all_servers_for_hash,
                });
                if pending_uploads.len() >= UPLOAD_CHECK_BATCH_SIZE
                    && !flush_pending_uploads(
                        &mut pending_uploads,
                        &blossom,
                        &all_servers,
                        use_upload_check,
                        !servers_needing_full.is_empty(),
                        &mut upload_check_supported,
                        &tx,
                        &counters,
                    )
                    .await
                {
                    break;
                }
            }

            let _ = flush_pending_uploads(
                &mut pending_uploads,
                &blossom,
                &all_servers,
                use_upload_check,
                !servers_needing_full.is_empty(),
                &mut upload_check_supported,
                &tx,
                &counters,
            )
            .await;

            discovery_complete.store(true, Ordering::Relaxed);

            let final_total_seen = discovered_total.load(Ordering::Relaxed);
            emit_upload_progress(upload_progress(
                completed.load(Ordering::Relaxed),
                final_total_seen,
                Some(final_total_seen),
                uploaded.load(Ordering::Relaxed),
                skipped_diff.load(Ordering::Relaxed),
                skipped_server.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
                has_old_tree,
            ));

            drop(tx);
            let _ = upload_handle.await;
            let _ = progress_done_tx.send(true);
            let _ = progress_handle.await;

            let final_uploaded = uploaded.load(Ordering::Relaxed);
            let final_skipped_diff = skipped_diff.load(Ordering::Relaxed);
            let final_skipped_server = skipped_server.load(Ordering::Relaxed);
            let final_failed = failed.load(Ordering::Relaxed);
            let final_local_failed = local_failed.load(Ordering::Relaxed);
            let final_completed = completed.load(Ordering::Relaxed);

            emit_upload_progress(upload_progress(
                final_completed,
                final_total_seen,
                Some(final_total_seen),
                final_uploaded,
                final_skipped_diff,
                final_skipped_server,
                final_failed,
                has_old_tree,
            ));
            eprintln!();

            info!(
                "Blossom upload complete: {} uploaded, {} unchanged (diff), {} already on server, {} failed",
                final_uploaded, final_skipped_diff, final_skipped_server, final_failed
            );

            let local_complete = final_local_failed == 0 && final_completed == final_total_seen;
            let degraded_replication = final_failed > final_local_failed;
            (local_complete, degraded_replication)
        });

        if local_complete {
            BlossomResult {
                configured: configured.clone(),
                succeeded: if degraded_replication {
                    vec![]
                } else {
                    configured.clone()
                },
                failed: if degraded_replication {
                    configured
                } else {
                    vec![]
                },
                local_complete: true,
                degraded: degraded_replication,
            }
        } else {
            BlossomResult {
                configured: configured.clone(),
                succeeded: vec![],
                failed: configured,
                local_complete: false,
                degraded: true,
            }
        }
    }

    /// Collect all hashes reachable from a root hash by walking the merkle tree
    #[allow(dead_code)]
    pub(super) fn collect_tree_hashes(&self, root_hash: &str) -> Result<Vec<[u8; 32]>> {
        use hashtree_core::try_decode_tree_node;

        let store = self.storage.store();
        let mut hashes = Vec::new();
        let mut visited: HashSet<[u8; 32]> = HashSet::new();

        let root_bytes = hex::decode(root_hash).context("Invalid root hash hex")?;
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

            if let Ok(Some(data)) = store.get_sync(&hash) {
                if let Some(node) = try_decode_tree_node(&data) {
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            queue.push(link.hash);
                        }
                    }
                }
            }
        }

        debug!(
            "Collected {} hashes from tree {}",
            hashes.len(),
            &root_hash[..12]
        );
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        batch_upload_retry_delay, effective_upload_concurrency, git_batch_upload_target_bytes,
        upload_progress_from_counters, UploadCounters,
    };
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[test]
    fn upload_concurrency_uses_configured_parallelism_for_single_server() {
        assert_eq!(effective_upload_concurrency(1, 10), 10);
    }

    #[test]
    fn upload_concurrency_clamps_zero_to_one() {
        assert_eq!(effective_upload_concurrency(1, 0), 1);
        assert_eq!(effective_upload_concurrency(0, 10), 1);
    }

    #[test]
    fn git_batch_upload_target_bytes_stays_below_protocol_max() {
        assert_eq!(git_batch_upload_target_bytes(), 8 * 1024 * 1024);
        assert!(git_batch_upload_target_bytes() <= hashtree_blossom::BATCH_UPLOAD_MAX_BYTES);
    }

    #[test]
    fn batch_upload_retry_delay_caps_exponential_backoff() {
        assert_eq!(batch_upload_retry_delay(0), Duration::from_secs(1));
        assert_eq!(batch_upload_retry_delay(1), Duration::from_secs(2));
        assert_eq!(batch_upload_retry_delay(2), Duration::from_secs(4));
        assert_eq!(batch_upload_retry_delay(99), Duration::from_secs(8));
    }

    #[test]
    fn upload_progress_snapshot_shows_known_total_after_discovery_completes() {
        let counters = UploadCounters {
            uploaded: Arc::new(AtomicUsize::new(2)),
            skipped_diff: Arc::new(AtomicUsize::new(1)),
            skipped_server: Arc::new(AtomicUsize::new(1)),
            failed: Arc::new(AtomicUsize::new(0)),
            completed: Arc::new(AtomicUsize::new(4)),
            discovered_total: Arc::new(AtomicUsize::new(9)),
        };
        let discovery_complete = AtomicBool::new(false);

        assert_eq!(
            upload_progress_from_counters(&counters, &discovery_complete, true).format(),
            "  Uploading: 4/? (9 discovered, 2 new, 1 unchanged, 1 exist)"
        );

        discovery_complete.store(true, Ordering::Relaxed);

        assert_eq!(
            upload_progress_from_counters(&counters, &discovery_complete, true).format(),
            "  Uploading: 4/9 (2 new, 1 unchanged, 1 exist)"
        );
    }
}
