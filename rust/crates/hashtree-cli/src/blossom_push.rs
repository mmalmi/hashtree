use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use hashtree_blossom::{BatchUploadItem, BlossomClient, BATCH_UPLOAD_MAX_BLOBS};
use hashtree_core::{to_hex, Cid, HashTree, HashTreeConfig, HashTreeError, Link, TreeNode};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::config::ensure_keys_string;
use crate::fetch::{child_cid, FetchConfig, Fetcher, WalkContext};
use crate::HashtreeStore;

const BLOSSOM_PUSH_BATCH_CONCURRENCY: usize = 4;
const BLOSSOM_PUSH_SINGLE_CONCURRENCY: usize = 8;
const BLOSSOM_PUSH_BATCH_TARGET_BYTES: usize = 8 * 1024 * 1024;
const BLOSSOM_PUSH_PROGRESS_EVERY: usize = 512;

fn parse_root_cid(cid_str: &str) -> Result<Cid> {
    Cid::parse(cid_str).map_err(|e| anyhow::anyhow!("Invalid CID '{}': {}", cid_str, e))
}

async fn load_local_blob_for_push(
    store: &HashtreeStore,
    fetcher: Option<&Fetcher>,
    cid: &Cid,
) -> Result<Vec<u8>> {
    if let Some(data) = store.get_blob(&cid.hash)? {
        if hashtree_core::sha256(&data) == cid.hash {
            return Ok(data);
        }
        anyhow::ensure!(
            fetcher.is_some(),
            "incorrect bytes in local store for {}",
            cid
        );
    }

    if let Some(fetcher) = fetcher {
        let data = fetcher
            .fetch_chunk(&to_hex(&cid.hash))
            .await
            .with_context(|| format!("failed to hydrate missing local blob {}", cid))?;
        store
            .put_blob(&data)
            .with_context(|| format!("failed to persist hydrated blob {}", cid))?;
        return Ok(data);
    }

    anyhow::bail!("missing local blob while pushing DAG: {}", cid);
}

async fn nodes_for_push(
    store: &HashtreeStore,
    fetcher: Option<&Fetcher>,
    cid: &Cid,
    context: WalkContext,
    data: Vec<u8>,
) -> Result<[Option<TreeNode>; 2]> {
    let physical = context.decode(cid, data.into())?;
    let Some(wrapper) = physical
        .as_ref()
        .filter(|node| context.is_wrapped_directory(node))
    else {
        return Ok([physical, None]);
    };
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let directory = match tree.read_wrapped_directory(cid, wrapper).await {
        Err(HashTreeError::MissingChunk(_)) if fetcher.is_some() => {
            // Fetch the physical file first. Its decoded entries are visited
            // by this collector under their own CID and file/directory context.
            fetcher.unwrap().fetch_cid_tree(store, cid).await?;
            tree.read_wrapped_directory(cid, wrapper).await?
        }
        result => result?,
    };
    Ok([physical, Some(directory)])
}

pub(crate) async fn collect_cids_for_push(
    store: &HashtreeStore,
    root_cid: Cid,
    fetcher: Option<&Fetcher>,
) -> Result<Vec<Cid>> {
    let mut cids_to_push = Vec::new();
    // A shared CID can be both literal file bytes and a structural node.
    // Traverse each interpretation, but upload each block only once.
    let mut visited = HashSet::new();
    let mut collected = HashSet::new();
    let mut queue = vec![(root_cid, WalkContext::Node)];

    while let Some((cid, context)) = queue.pop() {
        if !visited.insert((cid.hash, cid.key, context)) {
            continue;
        }

        let data = load_local_blob_for_push(store, fetcher, &cid).await?;
        if collected.insert(cid.hash) {
            cids_to_push.push(cid.clone());
        }

        for node in nodes_for_push(store, fetcher, &cid, context, data)
            .await?
            .into_iter()
            .flatten()
        {
            for link in &node.links {
                queue.push((
                    child_cid(&cid, link),
                    WalkContext::for_link(node.node_type, link),
                ));
            }
        }
    }

    Ok(cids_to_push)
}

fn matching_old_child<'a>(
    old_links: &'a [Link],
    new_index: usize,
    new_link: &Link,
) -> Option<&'a Link> {
    if let Some(name) = new_link.name.as_deref() {
        old_links
            .iter()
            .find(|old_link| old_link.name.as_deref() == Some(name))
    } else {
        old_links
            .get(new_index)
            .filter(|old_link| old_link.name.is_none())
    }
}

pub(crate) async fn collect_incremental_cids_for_push(
    store: &HashtreeStore,
    root_cid: Cid,
    previous_root_cid: Cid,
    fetcher: Option<&Fetcher>,
) -> Result<Vec<Cid>> {
    collect_incremental_cids(store, root_cid, previous_root_cid, fetcher, false).await
}

async fn collect_incremental_cids(
    store: &HashtreeStore,
    root_cid: Cid,
    previous_root_cid: Cid,
    fetcher: Option<&Fetcher>,
    require_previous_nodes: bool,
) -> Result<Vec<Cid>> {
    let mut cids_to_push = Vec::new();
    let mut visited_new = HashSet::new();
    let mut collected = HashSet::new();
    let mut queue = vec![(root_cid, WalkContext::Node, Some(previous_root_cid))];

    while let Some((cid, context, previous)) = queue.pop() {
        if previous.as_ref() == Some(&cid) {
            continue;
        }
        if !visited_new.insert((cid.hash, cid.key, context)) {
            continue;
        }

        let data = load_local_blob_for_push(store, fetcher, &cid).await?;
        if collected.insert(cid.hash) {
            cids_to_push.push(cid.clone());
        }

        let nodes = nodes_for_push(store, fetcher, &cid, context, data).await?;
        if nodes[0].is_none() {
            continue;
        }

        let old_node = match previous.as_ref() {
            Some(old_cid) => {
                let old_bytes = if require_previous_nodes {
                    Some(
                        load_local_blob_for_push(store, fetcher, old_cid)
                            .await
                            .context("previous DAG is unavailable for a bounded delta push")?,
                    )
                } else {
                    store
                        .get_blob(&old_cid.hash)?
                        .filter(|bytes| hashtree_core::sha256(bytes) == old_cid.hash)
                };
                match old_bytes {
                    Some(old_bytes) => match nodes_for_push(
                        store,
                        fetcher.filter(|_| require_previous_nodes),
                        old_cid,
                        context,
                        old_bytes,
                    )
                    .await
                    {
                        Ok(old_nodes) => Some((old_cid, old_nodes)),
                        Err(err) if require_previous_nodes => {
                            return Err(err)
                                .context("previous DAG is unreadable for a bounded delta push");
                        }
                        Err(err) => {
                            tracing::warn!(
                                "Failed to inspect previous Blossom DAG node {}; uploading changed subtree: {}",
                                old_cid,
                                err
                            );
                            None
                        }
                    },
                    None => None,
                }
            }
            None => None,
        };

        for (part, node) in nodes.into_iter().enumerate() {
            let Some(node) = node else { continue };
            for (index, link) in node.links.iter().enumerate() {
                let child = child_cid(&cid, link);
                let child_context = WalkContext::for_link(node.node_type, link);
                // A previous literal reference provides no coverage for a subtree
                // reached through a different interpretation of the same bytes.
                let old_child = old_node.as_ref().and_then(|(old_cid, old_nodes)| {
                    let old_node = old_nodes[part].as_ref()?;
                    matching_old_child(&old_node.links, index, link)
                        .filter(|old_link| {
                            WalkContext::for_link(old_node.node_type, old_link) == child_context
                        })
                        .map(|old_link| child_cid(old_cid, old_link))
                });
                if old_child.as_ref() != Some(&child) {
                    queue.push((child, child_context, old_child));
                }
            }
        }
    }

    Ok(cids_to_push)
}

enum BatchOutcome {
    Done { uploaded: usize, existing: usize },
    Unsupported,
}

async fn upload_batch_adaptive(
    client: &BlossomClient,
    server: &str,
    items: Vec<BatchUploadItem>,
) -> Result<BatchOutcome> {
    let mut stack = vec![items];
    let mut uploaded = 0usize;
    let mut existing = 0usize;

    while let Some(mut batch) = stack.pop() {
        let mut last_err = None;
        let attempts = if batch.len() > 1 { 2 } else { 3 };
        let mut settled = false;
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
            }
            match client.upload_batch_to_server(server, &batch).await {
                Ok(Some(res)) => {
                    uploaded += res.uploaded;
                    existing += res.accepted - res.uploaded;
                    settled = true;
                    break;
                }
                Ok(None) => return Ok(BatchOutcome::Unsupported),
                Err(err) => last_err = Some(err),
            }
        }
        if settled {
            continue;
        }
        if batch.len() > 1 {
            let right = batch.split_off(batch.len() / 2);
            stack.push(right);
            stack.push(batch);
        } else {
            return Err(anyhow::anyhow!(
                "{}",
                last_err.expect("batch attempt recorded an error")
            ));
        }
    }

    Ok(BatchOutcome::Done { uploaded, existing })
}

#[derive(Default)]
struct PushStats {
    uploaded: usize,
    skipped: usize,
    errors: usize,
    last_error: Option<String>,
}

impl PushStats {
    fn merge(&mut self, other: Self) {
        self.uploaded += other.uploaded;
        self.skipped += other.skipped;
        self.errors += other.errors;
        if other.last_error.is_some() {
            self.last_error = other.last_error;
        }
    }
}

enum SinglePushTarget {
    Loaded(Vec<u8>),
    Cid(Cid),
}

async fn take_next_batch(
    pending: &mut std::collections::VecDeque<Cid>,
    store: &HashtreeStore,
    fetcher: Option<&Fetcher>,
) -> Result<Vec<BatchUploadItem>> {
    let mut batch = Vec::new();
    let mut bytes = 0usize;

    while batch.len() < BATCH_UPLOAD_MAX_BLOBS {
        let Some(cid) = pending.front() else {
            break;
        };
        if !batch.is_empty() {
            let next_len = store.blob_size(&cid.hash)?.unwrap_or(0) as usize;
            if bytes.saturating_add(next_len) > BLOSSOM_PUSH_BATCH_TARGET_BYTES {
                break;
            }
        }
        let cid = pending.pop_front().expect("checked non-empty");
        let data = load_local_blob_for_push(store, fetcher, &cid).await?;
        bytes = bytes.saturating_add(data.len());
        batch.push(BatchUploadItem::new(to_hex(&cid.hash), data));
        if bytes >= BLOSSOM_PUSH_BATCH_TARGET_BYTES {
            break;
        }
    }

    Ok(batch)
}

async fn upload_singles(
    store: Arc<HashtreeStore>,
    fetcher: Option<Arc<Fetcher>>,
    client: BlossomClient,
    preloaded: Vec<BatchUploadItem>,
    cids: Vec<Cid>,
    force_upload: bool,
    servers_override: Option<Vec<String>>,
) -> PushStats {
    let targets = preloaded
        .into_iter()
        .map(|item| SinglePushTarget::Loaded(item.data))
        .chain(cids.into_iter().map(SinglePushTarget::Cid));

    let uploads = stream::iter(targets.map(|target| {
        let store = Arc::clone(&store);
        let fetcher = fetcher.clone();
        let client = client.clone();
        let servers_override = servers_override.clone();
        async move {
            let data = match target {
                SinglePushTarget::Loaded(data) => data,
                SinglePushTarget::Cid(cid) => {
                    load_local_blob_for_push(store.as_ref(), fetcher.as_deref(), &cid).await?
                }
            };
            if force_upload {
                let targets = servers_override
                    .as_deref()
                    .unwrap_or_else(|| client.write_servers());
                client
                    .upload_to_selected_servers(&data, targets)
                    .await
                    .map(|(hash, _)| (hash, true))
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            } else {
                client
                    .upload_if_missing(&data)
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            }
        }
    }))
    .buffer_unordered(BLOSSOM_PUSH_SINGLE_CONCURRENCY);

    collect_single_results(uploads).await
}

async fn collect_single_results(
    mut uploads: impl futures::Stream<Item = Result<(String, bool)>> + Unpin,
) -> PushStats {
    let mut stats = PushStats::default();
    while let Some(result) = uploads.next().await {
        match result {
            Ok((_hash, true)) => stats.uploaded += 1,
            Ok((_hash, false)) => stats.skipped += 1,
            Err(error) => {
                tracing::warn!("Blossom upload failed: {}", error);
                stats.errors += 1;
                stats.last_error = Some(error.to_string());
            }
        }
    }
    stats
}

async fn upload_regular_batch(
    client: &BlossomClient,
    servers: &[String],
    batch: Vec<BatchUploadItem>,
) -> PushStats {
    let mut pending = vec![batch];
    let mut stats = PushStats::default();
    'next_batch: while let Some(mut batch) = pending.pop() {
        let mut too_large = false;
        for server in servers {
            match client.upload_batch_to_server(server, &batch).await {
                Ok(Some(receipt)) => {
                    stats.uploaded += receipt.uploaded;
                    stats.skipped += receipt.accepted - receipt.uploaded;
                    continue 'next_batch;
                }
                Ok(None) => {}
                Err(hashtree_blossom::BlossomError::Http(err))
                    if err.status() == Some(reqwest::StatusCode::PAYLOAD_TOO_LARGE) =>
                {
                    too_large = true;
                }
                Err(err) => tracing::warn!("Blossom batch upload to {server} failed: {err}"),
            }
        }
        if too_large && batch.len() > 1 {
            pending.push(batch.split_off(batch.len() / 2));
            pending.push(batch);
            continue;
        }

        // Plain Blossom servers and single-upload endpoints remain usable
        // when all batch endpoints decline this range.
        let uploads = stream::iter(batch.into_iter().map(|item| async move {
            client
                .upload_if_missing(&item.data)
                .await
                .map_err(anyhow::Error::from)
        }))
        .buffer_unordered(BLOSSOM_PUSH_SINGLE_CONCURRENCY / BLOSSOM_PUSH_BATCH_CONCURRENCY);
        stats.merge(collect_single_results(uploads).await);
    }
    stats
}

#[derive(Clone, Copy)]
enum BatchTargets<'a> {
    Any(&'a [String]),
    One(&'a str),
}

impl BatchTargets<'_> {
    async fn upload(self, client: &BlossomClient, batch: Vec<BatchUploadItem>) -> PushStats {
        let server = match self {
            Self::Any(servers) => return upload_regular_batch(client, servers, batch).await,
            Self::One(server) => server,
        };
        let len = batch.len();
        let error = match upload_batch_adaptive(client, server, batch).await {
            Ok(BatchOutcome::Done { uploaded, existing }) => {
                return PushStats {
                    uploaded,
                    skipped: existing,
                    ..Default::default()
                }
            }
            Ok(BatchOutcome::Unsupported) => "server stopped accepting batch uploads".to_string(),
            Err(err) => err.to_string(),
        };
        tracing::warn!("Blossom batch upload failed: {error}");
        PushStats {
            errors: len,
            last_error: Some(error),
            ..Default::default()
        }
    }
}

async fn upload_batches(
    store: &HashtreeStore,
    fetcher: Option<&Fetcher>,
    client: &BlossomClient,
    targets: BatchTargets<'_>,
    pending: &mut std::collections::VecDeque<Cid>,
    total: usize,
) -> Result<PushStats> {
    use futures::stream::FuturesUnordered;

    let mut stats = PushStats::default();
    let mut processed = 0;
    let mut next_report = BLOSSOM_PUSH_PROGRESS_EVERY;
    let mut in_flight = FuturesUnordered::new();

    loop {
        while in_flight.len() < BLOSSOM_PUSH_BATCH_CONCURRENCY && !pending.is_empty() {
            let batch = take_next_batch(pending, store, fetcher).await?;
            if batch.is_empty() {
                break;
            }
            let len = batch.len();
            in_flight.push(async move { (len, targets.upload(client, batch).await) });
        }

        let Some((len, result)) = in_flight.next().await else {
            break;
        };
        processed += len;
        stats.merge(result);
        if total > 0 && processed >= next_report && processed < total {
            println!(
                "  file servers: {processed}/{total} processed ({} uploaded, {} already exist, {} failed)",
                stats.uploaded,
                stats.skipped,
                stats.errors,
            );
            next_report =
                ((processed / BLOSSOM_PUSH_PROGRESS_EVERY) + 1) * BLOSSOM_PUSH_PROGRESS_EVERY;
        }
    }

    Ok(stats)
}

async fn upload_cids_with_client(
    store: Arc<HashtreeStore>,
    fetcher: Option<Arc<Fetcher>>,
    client: BlossomClient,
    cids_to_push: Vec<Cid>,
    force_upload: bool,
) -> Result<(usize, usize)> {
    use std::collections::VecDeque;

    let total = cids_to_push.len();
    if total == 0 {
        return Ok((0, 0));
    }

    let mut stats = PushStats::default();

    let pending_cids = cids_to_push;
    let client = client.with_confirmed_uploads();
    if !force_upload {
        let probe = [to_hex(&pending_cids[0].hash)];
        let mut batch_servers = Vec::new();
        for server in client.write_servers() {
            // Global presence only identifies the batch API. A signed upload
            // lets the server record this publisher even for existing bytes.
            if client
                .check_uploads_on_server(&probe, server)
                .await
                .is_some()
            {
                batch_servers.push(server.clone());
            }
        }
        if batch_servers.is_empty() {
            stats = upload_singles(
                store,
                fetcher,
                client,
                Vec::new(),
                pending_cids,
                false,
                None,
            )
            .await;
        } else {
            let mut pending = pending_cids.into();
            stats = upload_batches(
                &store,
                fetcher.as_deref(),
                &client,
                BatchTargets::Any(&batch_servers),
                &mut pending,
                total,
            )
            .await?;
        }
    } else {
        // force_upload pushes every blob to all configured write servers.
        // A one-blob batch can commit a shallow root on an optimistic PUT server.
        let item_count = pending_cids.len();
        let probe_hash = vec![to_hex(&pending_cids[0].hash)];
        let mut any_server_succeeded = false;
        for server in client.write_servers().to_vec() {
            let supports_batch = client
                .check_uploads_on_server(&probe_hash, &server)
                .await
                .is_some();
            if !supports_batch {
                let single = upload_singles(
                    Arc::clone(&store),
                    fetcher.clone(),
                    client.clone(),
                    Vec::new(),
                    pending_cids.clone(),
                    true,
                    Some(vec![server]),
                )
                .await;
                if single.errors == 0 {
                    any_server_succeeded = true;
                } else {
                    stats.last_error = single.last_error;
                }
                continue;
            }
            let mut pending: VecDeque<Cid> = pending_cids.iter().cloned().collect();
            let first = take_next_batch(&mut pending, store.as_ref(), fetcher.as_deref()).await?;
            match upload_batch_adaptive(&client, &server, first.clone()).await {
                Ok(BatchOutcome::Done { .. }) => {
                    let rest = upload_batches(
                        store.as_ref(),
                        fetcher.as_deref(),
                        &client,
                        BatchTargets::One(&server),
                        &mut pending,
                        0,
                    )
                    .await?;
                    if rest.errors == 0 {
                        any_server_succeeded = true;
                    } else {
                        stats.last_error = rest.last_error;
                    }
                }
                Ok(BatchOutcome::Unsupported) => {
                    let single = upload_singles(
                        Arc::clone(&store),
                        fetcher.clone(),
                        client.clone(),
                        first,
                        pending.into(),
                        true,
                        Some(vec![server]),
                    )
                    .await;
                    if single.errors == 0 {
                        any_server_succeeded = true;
                    } else {
                        stats.last_error = single.last_error;
                    }
                }
                Err(err) => {
                    stats.last_error = Some(err.to_string());
                }
            }
        }
        if any_server_succeeded {
            stats.uploaded += item_count;
        } else {
            stats.errors += item_count;
        }
    }

    println!(
        "  file servers: {total}/{total} processed ({} uploaded, {} already exist, {} failed)",
        stats.uploaded, stats.skipped, stats.errors,
    );

    if stats.errors > 0 {
        let detail = stats
            .last_error
            .as_deref()
            .map(|err| format!(" (last error: {err})"))
            .unwrap_or_default();
        anyhow::bail!(
            "failed to upload {} blob(s) to configured file servers{}",
            stats.errors,
            detail
        );
    }

    Ok((stats.uploaded, stats.skipped))
}

/// Push content to Blossom servers.
pub async fn push_to_blossom(
    data_dir: &Path,
    cid_str: &str,
    server_override: Option<String>,
    force_upload: bool,
    shallow: bool,
    previous_root: Option<&str>,
) -> Result<()> {
    use nostr::Keys;

    if shallow && previous_root.is_some() {
        anyhow::bail!("a DAG delta cannot be combined with a shallow push");
    }

    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    let client = if let Some(server) = server_override {
        BlossomClient::new(keys).with_write_servers(vec![server])
    } else {
        BlossomClient::new(keys)
    };

    if client.write_servers().is_empty() {
        anyhow::bail!(
            "No file servers configured. Use --server or add write_servers to config.toml"
        );
    }

    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let fetcher = Arc::new(Fetcher::new(FetchConfig::default()));

    let root_cid = parse_root_cid(cid_str)?;
    let cids_to_push = if shallow {
        load_local_blob_for_push(store.as_ref(), Some(fetcher.as_ref()), &root_cid).await?;
        vec![root_cid]
    } else if let Some(previous_root) = previous_root {
        let previous = parse_root_cid(previous_root)?;
        load_local_blob_for_push(store.as_ref(), Some(fetcher.as_ref()), &previous).await?;
        println!("Collecting blocks changed since the retained previous root...");
        // The caller must already retain the previous DAG on this server. A
        // collection error aborts this push rather than restarting a full scan.
        collect_incremental_cids(
            store.as_ref(),
            root_cid,
            previous,
            Some(fetcher.as_ref()),
            true,
        )
        .await?
    } else {
        println!("Collecting blocks...");
        collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?
    };

    println!("Found {} blocks to push", cids_to_push.len());
    let (uploaded, skipped) =
        upload_cids_with_client(store, Some(fetcher), client, cids_to_push, force_upload).await?;

    println!("\nUploaded: {uploaded}, Skipped: {skipped}, Errors: 0");
    println!("Done!");
    Ok(())
}

/// Push tree to Blossom servers using BlossomClient.
pub async fn background_blossom_push(
    data_dir: &Path,
    cid_str: &str,
    servers: &[String],
) -> Result<()> {
    let store = Arc::new(HashtreeStore::new(data_dir)?);
    background_blossom_push_with_store(store, cid_str, servers).await
}

pub async fn background_blossom_push_with_store(
    store: Arc<HashtreeStore>,
    cid_str: &str,
    servers: &[String],
) -> Result<()> {
    let root_cid = parse_root_cid(cid_str)?;
    background_blossom_push_incremental_with_store(store, root_cid, None, servers).await
}

pub async fn background_blossom_push_incremental_with_store(
    store: Arc<HashtreeStore>,
    root_cid: Cid,
    previous_root_cid: Option<Cid>,
    servers: &[String],
) -> Result<()> {
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    let fetcher = Arc::new(Fetcher::new(FetchConfig::default()));
    let cids_to_push = if let Some(previous_root_cid) = previous_root_cid.as_ref() {
        println!("Collecting bounded DAG diff for file-server push...");
        match collect_incremental_cids_for_push(
            store.as_ref(),
            root_cid.clone(),
            previous_root_cid.clone(),
            Some(fetcher.as_ref()),
        )
        .await
        {
            Ok(cids) => cids,
            Err(err) => {
                tracing::warn!(
                    "Blossom DAG diff failed; falling back to full push: {}",
                    err
                );
                collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?
            }
        }
    } else {
        println!("Collecting DAG for file-server push...");
        collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?
    };

    if cids_to_push.is_empty() {
        return Ok(());
    }

    let client = if servers.is_empty() {
        BlossomClient::new(keys)
    } else {
        BlossomClient::new(keys).with_write_servers(servers.to_vec())
    };
    upload_cids_with_client(store, Some(fetcher), client, cids_to_push, false).await?;
    Ok(())
}

#[cfg(test)]
mod manifest_tests;

#[cfg(test)]
mod regular_push_tests;

#[cfg(test)]
mod tests {
    use super::{collect_cids_for_push, collect_incremental_cids_for_push};
    use crate::HashtreeStore;
    use futures::executor::block_on as sync_block_on;
    use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType};
    use tempfile::tempdir;

    #[tokio::test]
    async fn push_collectors_accept_tree_shaped_raw_blobs() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let payload = [0x82, 0xa1, b'l', 0x90, 0xa1, b't', 0x40];

        for encrypted in [false, true] {
            let mut config = HashTreeConfig::new(store.store_arc());
            if !encrypted {
                config = config.public();
            }
            let tree = HashTree::new(config);
            let old_root = tree.put_directory(vec![]).await.expect("old root");
            let (blob, size) = tree.put(&payload).await.expect("raw blob");
            let root = tree
                .put_directory(vec![
                    DirEntry::from_cid("raw.msgpack", &blob).with_size(size)
                ])
                .await
                .expect("root");

            for previous in [None, Some(old_root)] {
                let cids = match previous {
                    Some(previous) => {
                        collect_incremental_cids_for_push(&store, root.clone(), previous, None)
                            .await
                    }
                    None => collect_cids_for_push(&store, root.clone(), None).await,
                }
                .expect("raw bytes must not be decoded as a structural node");
                let hashes = cids
                    .iter()
                    .map(|cid| cid.hash)
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    hashes,
                    std::collections::HashSet::from([root.hash, blob.hash])
                );
            }
        }
    }

    #[tokio::test]
    async fn push_collectors_walk_mixed_chunk_sizes_and_reject_missing_chunks() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let old_root = tree.put_directory(vec![]).await.expect("old root");
        let mut entries = Vec::new();
        let mut required_hashes = std::collections::HashSet::new();
        let mut missing_chunk = None;

        for chunk_size in [64 * 1024, hashtree_core::DEFAULT_CHUNK_SIZE] {
            let chunk_tree = HashTree::new(
                HashTreeConfig::new(store.store_arc())
                    .with_chunk_size(chunk_size)
                    .with_max_links(2),
            );
            let data = [
                vec![1; chunk_size],
                vec![2; chunk_size],
                vec![3; chunk_size],
                vec![4; 7],
            ]
            .concat();
            let (file, size) = chunk_tree.put(&data).await.expect("chunked file");
            required_hashes.insert(file.hash);
            for chunk in data.chunks(chunk_size) {
                let (leaf, _) = chunk_tree.put(chunk).await.expect("leaf");
                required_hashes.insert(leaf.hash);
                missing_chunk.get_or_insert(leaf.hash);
            }
            entries.push(DirEntry::from_cid(chunk_size.to_string(), &file).with_size(size));
        }
        let root = tree.put_directory(entries).await.expect("mixed root");
        required_hashes.insert(root.hash);

        for previous in [None, Some(old_root.clone())] {
            let cids = match previous {
                Some(previous) => {
                    collect_incremental_cids_for_push(&store, root.clone(), previous, None).await
                }
                None => collect_cids_for_push(&store, root.clone(), None).await,
            }
            .expect("all chunk sizes must be traversed");
            let hashes = cids
                .iter()
                .map(|cid| cid.hash)
                .collect::<std::collections::HashSet<_>>();
            assert!(required_hashes.is_subset(&hashes));
        }

        store
            .router()
            .delete_local_only(&missing_chunk.expect("leaf"))
            .expect("delete chunk");
        for previous in [None, Some(old_root)] {
            let result = match previous {
                Some(previous) => {
                    collect_incremental_cids_for_push(&store, root.clone(), previous, None).await
                }
                None => collect_cids_for_push(&store, root.clone(), None).await,
            };
            assert!(result
                .expect_err("missing chunk must fail")
                .to_string()
                .contains("missing local blob"));
        }
    }

    #[tokio::test]
    async fn push_collectors_reject_invalid_structural_nodes() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let old_root = tree.put_directory(vec![]).await.expect("old root");
        let (invalid, size) = tree
            .put(&[0x82, 0xa1, b'l', 0x90, 0xa1, b't', 0x40])
            .await
            .expect("invalid node");

        for kind in [LinkType::Dir, LinkType::File, LinkType::Fanout] {
            let root = tree
                .put_directory(vec![DirEntry::from_cid("invalid", &invalid)
                    .with_size(size)
                    .with_link_type(kind)])
                .await
                .expect("root");
            for previous in [None, Some(old_root.clone())] {
                let result = match previous {
                    Some(previous) => {
                        collect_incremental_cids_for_push(&store, root.clone(), previous, None)
                            .await
                    }
                    None => collect_cids_for_push(&store, root.clone(), None).await,
                };
                assert!(result
                    .expect_err("invalid structural node must fail")
                    .to_string()
                    .contains("Invalid node type: 64"));
            }
        }
    }

    #[tokio::test]
    async fn push_collectors_expand_nodes_also_referenced_as_raw_blobs() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        for encrypted in [false, true] {
            let mut config = HashTreeConfig::new(store.store_arc());
            if !encrypted {
                config = config.public();
            }
            let chunk_tree = HashTree::new(HashTreeConfig {
                chunk_size: 4,
                encrypted,
                ..HashTreeConfig::new(store.store_arc())
            });
            let tree = HashTree::new(config);
            let (file, size) = chunk_tree.put(b"multiple chunks").await.expect("file");
            let file_node = tree
                .get_node(&file)
                .await
                .expect("file node")
                .expect("node");
            let encoded = tree
                .get_blob(&file.hash)
                .await
                .expect("encoded file")
                .expect("bytes");
            let encoded = match file.key.as_ref() {
                Some(key) => hashtree_core::decrypt_object(&encoded, key).expect("decrypt file"),
                None => encoded,
            };
            let (raw, raw_size) = tree.put(&encoded).await.expect("raw encoded node");
            assert_eq!(file, raw, "both interpretations must share the same CID");
            let old_root = tree.put_directory(vec![]).await.expect("old root");
            let raw_only_root = tree
                .put_directory(vec![
                    DirEntry::from_cid("a-file", &raw).with_size(raw_size),
                    DirEntry::from_cid("z-raw", &raw).with_size(raw_size),
                ])
                .await
                .expect("raw-only root");
            let root = tree
                .put_directory(vec![
                    DirEntry::from_cid("a-file", &file)
                        .with_size(size)
                        .with_link_type(LinkType::File),
                    DirEntry::from_cid("z-raw", &raw).with_size(raw_size),
                ])
                .await
                .expect("root");
            let mut expected = std::collections::HashSet::from([root.hash, file.hash]);
            expected.extend(file_node.links.iter().map(|link| link.hash));

            for previous in [None, Some(old_root), Some(raw_only_root)] {
                let cids = match previous {
                    Some(previous) => {
                        collect_incremental_cids_for_push(&store, root.clone(), previous, None)
                            .await
                    }
                    None => collect_cids_for_push(&store, root.clone(), None).await,
                }
                .expect("structural reference must still expand after raw reference");
                let hashes = cids
                    .iter()
                    .map(|cid| cid.hash)
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(hashes, expected);
                assert_eq!(
                    cids.len(),
                    hashes.len(),
                    "upload each shared block only once"
                );
            }
        }
    }

    #[tokio::test]
    async fn incremental_push_does_not_prune_children_of_old_raw_blobs() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        for encrypted in [false, true] {
            let config = || HashTreeConfig {
                encrypted,
                ..HashTreeConfig::new(store.store_arc())
            };
            let tree = HashTree::new(config());
            let chunk_tree = HashTree::new(config().with_chunk_size(4));
            let (shared, _) = chunk_tree.put(b"abcd").await.expect("shared leaf");
            let (file, _) = chunk_tree.put(b"abcdabcd").await.expect("file node");
            let encoded = tree
                .get_blob(&file.hash)
                .await
                .expect("encoded node")
                .expect("bytes");
            let encoded = match file.key.as_ref() {
                Some(key) => hashtree_core::decrypt_object(&encoded, key).expect("decrypt node"),
                None => encoded,
            };
            let (raw, size) = tree.put(&encoded).await.expect("literal node bytes");
            let old_root = tree
                .put_directory(vec![DirEntry::from_cid("file", &raw).with_size(size)])
                .await
                .expect("old root");
            let old_cids = collect_cids_for_push(&store, old_root.clone(), None)
                .await
                .expect("raw-only closure");
            assert!(!old_cids.iter().any(|cid| cid.hash == shared.hash));

            let mut data = vec![b'x'; size as usize];
            data[..4].copy_from_slice(b"abcd");
            let (new_file, new_size) = chunk_tree.put(&data).await.expect("new chunked file");
            assert_eq!(new_size, size);
            assert_ne!(new_file.hash, raw.hash);
            let new_root = tree
                .put_directory(vec![
                    DirEntry::from_cid("file", &new_file).with_size(new_size)
                ])
                .await
                .expect("new root");
            let cids = collect_incremental_cids_for_push(&store, new_root, old_root, None)
                .await
                .expect("changed closure");
            assert!(
                cids.iter().any(|cid| cid.hash == shared.hash),
                "old raw bytes provide no descendant coverage"
            );
        }
    }

    #[tokio::test]
    async fn failed_http_uploads_return_an_error() {
        let tmp = tempdir().expect("tempdir");
        let store = std::sync::Arc::new(
            HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store"),
        );
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let (cid, _) = tree.put(b"upload rejection fixture").await.expect("blob");
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_requests = requests.clone();
        let app = axum::Router::new().route(
            "/upload",
            axum::routing::put(move |body: bytes::Bytes| async move {
                assert!(!body.is_empty());
                observed_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                axum::http::StatusCode::BAD_REQUEST
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let client = hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
            .with_write_servers(vec![format!("http://{address}")]);

        for force_upload in [false, true] {
            let result = super::upload_cids_with_client(
                store.clone(),
                None,
                client.clone(),
                vec![cid.clone()],
                force_upload,
            )
            .await;
            assert!(result
                .expect_err("failed uploads must not report success")
                .to_string()
                .contains("failed to upload 1 blob(s)"));
        }
        assert_eq!(requests.load(std::sync::atomic::Ordering::Relaxed), 2);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn collect_cids_for_push_fails_on_missing_descendant_blob() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let root = sync_block_on(async {
            let (file_cid, _size) = tree.put_file(b"hello").await.expect("file");
            tree.put_directory(vec![hashtree_core::DirEntry::from_cid(
                "greeting.txt",
                &file_cid,
            )])
            .await
            .expect("dir")
        });

        let entries = store
            .get_tree_node(&root.hash)
            .expect("root node")
            .expect("root node present")
            .links;
        let child_hash = entries[0].hash;
        store
            .router()
            .delete_local_only(&child_hash)
            .expect("delete child locally");

        let err = collect_cids_for_push(&store, root, None)
            .await
            .expect_err("missing child should fail");
        assert!(err
            .to_string()
            .contains("missing local blob while pushing DAG"));
    }

    #[tokio::test]
    async fn explicit_delta_fails_when_a_previous_comparison_subtree_is_missing() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let old_child = tree.put_directory(vec![]).await.expect("old child");
        let old_root = tree
            .put_directory(vec![
                DirEntry::from_cid("subtree", &old_child).with_link_type(LinkType::Dir)
            ])
            .await
            .expect("old root");
        let leaf = tree.put_blob(b"new event").await.expect("leaf");
        let new_child = tree
            .put_directory(vec![DirEntry::new("event", leaf).with_size(9)])
            .await
            .expect("new child");
        let new_root = tree
            .put_directory(vec![
                DirEntry::from_cid("subtree", &new_child).with_link_type(LinkType::Dir)
            ])
            .await
            .expect("new root");
        store
            .router()
            .delete_local_only(&old_child.hash)
            .expect("remove old subtree");

        let error =
            super::collect_incremental_cids(&store, new_root.clone(), old_root.clone(), None, true)
                .await
                .expect_err("explicit deltas must not silently expand to a full traversal");
        assert!(error.to_string().contains("previous DAG is unavailable"));
        let fallback = collect_incremental_cids_for_push(&store, new_root, old_root, None)
            .await
            .expect("existing background behavior remains available");
        assert_eq!(fallback.len(), 3);
    }

    #[tokio::test]
    async fn incremental_push_collects_only_changed_named_subtrees() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let stable_file = tree.put_blob(b"stable").await.expect("stable file");
        let old_changed_file = tree.put_blob(b"old").await.expect("old file");
        let old_subdir = tree
            .put_directory(vec![
                DirEntry::new("changed.txt", old_changed_file).with_size(3),
                DirEntry::new("stable.txt", stable_file).with_size(6),
            ])
            .await
            .expect("old subdir");
        let old_root = tree
            .put_directory(vec![
                DirEntry::new("subdir", old_subdir.hash).with_link_type(LinkType::Dir),
                DirEntry::new("stable-root.txt", stable_file).with_size(6),
            ])
            .await
            .expect("old root");

        let new_changed_file = tree.put_blob(b"new").await.expect("new file");
        let new_subdir = tree
            .put_directory(vec![
                DirEntry::new("stable.txt", stable_file).with_size(6),
                DirEntry::new("changed.txt", new_changed_file).with_size(3),
            ])
            .await
            .expect("new subdir");
        let new_root = tree
            .put_directory(vec![
                DirEntry::new("stable-root.txt", stable_file).with_size(6),
                DirEntry::new("subdir", new_subdir.hash).with_link_type(LinkType::Dir),
            ])
            .await
            .expect("new root");

        let cids = collect_incremental_cids_for_push(&store, new_root.clone(), old_root, None)
            .await
            .expect("incremental cids");
        let hashes = cids.iter().map(|cid| cid.hash).collect::<Vec<_>>();

        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&new_root.hash));
        assert!(hashes.contains(&new_subdir.hash));
        assert!(hashes.contains(&new_changed_file));
        assert!(!hashes.contains(&stable_file));
    }

    #[tokio::test]
    async fn upload_cids_uses_check_and_binary_batch_with_single_put_fallback() {
        use base64::Engine;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let tmp = tempdir().unwrap();
        let store =
            Arc::new(HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).unwrap());
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let mut cids = Vec::new();
        for i in 0..5u8 {
            let (cid, _) = tree.put(&vec![i + 1; 512]).await.unwrap();
            cids.push(cid);
        }
        let preseeded = hashtree_core::to_hex(&cids[0].hash);

        let check_reqs = Arc::new(AtomicUsize::new(0));
        let batch_reqs = Arc::new(AtomicUsize::new(0));
        let uploaded_count = Arc::new(AtomicUsize::new(0));
        let check_reqs_r = Arc::clone(&check_reqs);
        let batch_reqs_r = Arc::clone(&batch_reqs);
        let uploaded_r = Arc::clone(&uploaded_count);
        let preseeded_r = preseeded.clone();

        let batch_app = axum::Router::new()
            .route(
                "/upload/check",
                axum::routing::post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let check_reqs = Arc::clone(&check_reqs_r);
                    let preseeded = preseeded_r.clone();
                    async move {
                        check_reqs.fetch_add(1, Ordering::SeqCst);
                        let hashes = req["hashes"].as_array().cloned().unwrap_or_default();
                        let mut bits = vec![0u8; hashes.len().div_ceil(8)];
                        for (i, h) in hashes.iter().enumerate() {
                            if h.as_str() == Some(preseeded.as_str()) {
                                bits[i / 8] |= 1 << (i % 8);
                            }
                        }
                        axum::Json(serde_json::json!({
                            "count": hashes.len(),
                            "present": base64::engine::general_purpose::STANDARD.encode(bits),
                        }))
                    }
                }),
            )
            .route(
                "/upload/batch-binary",
                axum::routing::post(move |body: bytes::Bytes| {
                    let batch_reqs = Arc::clone(&batch_reqs_r);
                    let uploaded = Arc::clone(&uploaded_r);
                    let preseeded = preseeded.clone();
                    async move {
                        batch_reqs.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(&body[..8], b"HTBBV1\0\0");
                        let count = u32::from_be_bytes(body[8..12].try_into().unwrap()) as usize;
                        let mut cursor = 12;
                        let mut descriptors = Vec::with_capacity(count);
                        let mut inserted = 0;
                        for _ in 0..count {
                            let hash = hex::encode(&body[cursor..cursor + 32]);
                            inserted += usize::from(hash != preseeded);
                            cursor += 32;
                            let ct_len =
                                u16::from_be_bytes(body[cursor..cursor + 2].try_into().unwrap())
                                    as usize;
                            cursor += 2;
                            let data_len =
                                u64::from_be_bytes(body[cursor..cursor + 8].try_into().unwrap())
                                    as usize;
                            cursor += 8 + ct_len + data_len;
                            descriptors.push(serde_json::json!({ "sha256": hash }));
                        }
                        uploaded.fetch_add(count, Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "uploaded": inserted,
                            "blobs": descriptors,
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, batch_app).await.unwrap() });

        let client = hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
            .with_write_servers(vec![format!("http://{addr}")]);
        let (uploaded, skipped) =
            super::upload_cids_with_client(store.clone(), None, client, cids.clone(), false)
                .await
                .expect("batch push succeeds");
        assert_eq!((uploaded, skipped), (4, 1));
        assert_eq!(check_reqs.load(Ordering::SeqCst), 1);
        assert_eq!(batch_reqs.load(Ordering::SeqCst), 1);
        assert_eq!(uploaded_count.load(Ordering::SeqCst), 5);
        server.abort();
        let _ = server.await;

        // Plain Blossom server without /upload/check or /upload/batch* must fall back to PUT /upload
        // without sending wasted batch upload payloads.
        let single_puts = Arc::new(AtomicUsize::new(0));
        let wasted_batches = Arc::new(AtomicUsize::new(0));
        let single_puts_r = Arc::clone(&single_puts);
        let wasted_b1 = Arc::clone(&wasted_batches);
        let wasted_b2 = Arc::clone(&wasted_batches);
        let plain_app = axum::Router::new()
            .route(
                "/upload",
                axum::routing::put(move |_body: bytes::Bytes| {
                    let single_puts = Arc::clone(&single_puts_r);
                    async move {
                        single_puts.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::CREATED
                    }
                }),
            )
            .route(
                "/upload/batch-binary",
                axum::routing::post(move |_body: bytes::Bytes| {
                    let wasted = Arc::clone(&wasted_b1);
                    async move {
                        wasted.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::NOT_FOUND
                    }
                }),
            )
            .route(
                "/upload/batch",
                axum::routing::post(move |_body: bytes::Bytes| {
                    let wasted = Arc::clone(&wasted_b2);
                    async move {
                        wasted.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::NOT_FOUND
                    }
                }),
            );
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let server2 = tokio::spawn(async move { axum::serve(listener2, plain_app).await.unwrap() });
        let client2 = hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
            .with_write_servers(vec![format!("http://{addr2}")]);
        let (uploaded2, skipped2) =
            super::upload_cids_with_client(store, None, client2, cids, false)
                .await
                .expect("plain fallback succeeds");
        assert_eq!((uploaded2, skipped2), (5, 0));
        assert_eq!(single_puts.load(Ordering::SeqCst), 5);
        assert_eq!(
            wasted_batches.load(Ordering::SeqCst),
            0,
            "plain Blossom servers must not receive wasted batch bodies after /upload/check 404"
        );
        server2.abort();
        let _ = server2.await;
    }
}
