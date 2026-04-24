use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::config::ensure_keys_string;
use crate::fetch::{FetchConfig, Fetcher};
use crate::HashtreeStore;
use hashtree_core::{to_hex, Cid, HashTree, HashTreeConfig, Link};

const BLOSSOM_PUSH_CONCURRENCY: usize = 16;
const BLOSSOM_PUSH_PROGRESS_EVERY: usize = 512;

fn parse_root_cid(cid_str: &str) -> Result<Cid> {
    Cid::parse(cid_str).map_err(|e| anyhow::anyhow!("Invalid CID '{}': {}", cid_str, e))
}

fn child_cid(parent: &Cid, link: &Link) -> Cid {
    let inherits_parent_key = link
        .name
        .as_deref()
        .map(|name| {
            name.starts_with("_chunk_")
                || (name.starts_with('_') && name.chars().count() == 2 && link.link_type.is_tree())
        })
        .unwrap_or(false);

    Cid {
        hash: link.hash,
        key: link.key.or(if inherits_parent_key {
            parent.key
        } else {
            None
        }),
    }
}

async fn ensure_local_blob_for_push(
    store: &HashtreeStore,
    fetcher: Option<&Fetcher>,
    cid: &Cid,
) -> Result<()> {
    if store.get_blob(&cid.hash)?.is_some() {
        return Ok(());
    }

    if let Some(fetcher) = fetcher {
        let data = fetcher
            .fetch_chunk(None, &to_hex(&cid.hash))
            .await
            .with_context(|| format!("failed to hydrate missing local blob {}", cid))?;
        store
            .put_blob(&data)
            .with_context(|| format!("failed to persist hydrated blob {}", cid))?;
        if store.get_blob(&cid.hash)?.is_some() {
            return Ok(());
        }
    }

    anyhow::bail!("missing local blob while pushing DAG: {}", cid);
}

pub(crate) async fn collect_cids_for_push(
    store: &HashtreeStore,
    root_cid: Cid,
    fetcher: Option<&Fetcher>,
) -> Result<Vec<Cid>> {
    let mut cids_to_push = Vec::new();
    let mut visited: HashSet<[u8; 32]> = HashSet::new();
    let mut queue = vec![root_cid];
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    while let Some(cid) = queue.pop() {
        if !visited.insert(cid.hash) {
            continue;
        }

        ensure_local_blob_for_push(store, fetcher, &cid).await?;
        cids_to_push.push(cid.clone());

        let node = tree
            .get_node(&cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to inspect {}: {}", cid, e))?;

        if let Some(node) = node {
            for link in &node.links {
                if !visited.contains(&link.hash) {
                    queue.push(child_cid(&cid, link));
                }
            }
        }
    }

    Ok(cids_to_push)
}

async fn upload_cids_with_client(
    store: Arc<HashtreeStore>,
    fetcher: Option<Arc<Fetcher>>,
    client: hashtree_blossom::BlossomClient,
    cids_to_push: Vec<Cid>,
) -> Result<(usize, usize, usize, Option<String>)> {
    let total = cids_to_push.len();
    let mut total_uploaded = 0usize;
    let mut total_skipped = 0usize;
    let mut total_errors = 0usize;
    let mut last_error = None;
    let mut processed = 0usize;

    let mut uploads = stream::iter(cids_to_push.into_iter().map(|cid| {
        let store = Arc::clone(&store);
        let fetcher = fetcher.clone();
        let client = client.clone();
        async move {
            ensure_local_blob_for_push(store.as_ref(), fetcher.as_deref(), &cid).await?;
            let data = store
                .get_blob(&cid.hash)?
                .ok_or_else(|| anyhow::anyhow!("missing local blob while uploading {}", cid))?;
            client
                .upload_if_missing(&data)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        }
    }))
    .buffer_unordered(BLOSSOM_PUSH_CONCURRENCY);

    while let Some(result) = uploads.next().await {
        processed += 1;
        match result {
            Ok((_hash, was_uploaded)) => {
                if was_uploaded {
                    total_uploaded += 1;
                } else {
                    total_skipped += 1;
                }
            }
            Err(error) => {
                tracing::warn!("Blossom upload failed: {}", error);
                total_errors += 1;
                last_error = Some(error.to_string());
            }
        }

        if processed % BLOSSOM_PUSH_PROGRESS_EVERY == 0 || processed == total {
            println!(
                "  file servers: {processed}/{total} processed ({total_uploaded} uploaded, {total_skipped} already exist, {total_errors} failed)",
            );
        }
    }

    Ok((total_uploaded, total_skipped, total_errors, last_error))
}

/// Push content to Blossom servers.
pub async fn push_to_blossom(
    data_dir: &Path,
    cid_str: &str,
    server_override: Option<String>,
) -> Result<()> {
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

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

    println!("Collecting blocks...");
    let root_cid = parse_root_cid(cid_str)?;
    let cids_to_push =
        collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?;

    println!("Found {} blocks to push", cids_to_push.len());
    let (uploaded, skipped, errors, last_error) =
        upload_cids_with_client(store, Some(fetcher), client, cids_to_push).await?;

    println!(
        "\nUploaded: {}, Skipped: {}, Errors: {}",
        uploaded, skipped, errors
    );
    if let Some(last_error) = last_error {
        eprintln!("Last error: {last_error}");
    }
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
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    let root_cid = parse_root_cid(cid_str)?;
    let fetcher = Arc::new(Fetcher::new(FetchConfig::default()));
    println!("Collecting DAG for file-server push...");
    let cids_to_push =
        collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?;

    if cids_to_push.is_empty() {
        return Ok(());
    }

    let client = if servers.is_empty() {
        BlossomClient::new(keys)
    } else {
        BlossomClient::new(keys).with_write_servers(servers.to_vec())
    };
    let (_total_uploaded, _total_skipped, total_errors, last_error) =
        upload_cids_with_client(store, Some(fetcher), client, cids_to_push).await?;

    if total_errors > 0 {
        let detail = last_error
            .as_deref()
            .map(|err| format!(" (last error: {err})"))
            .unwrap_or_default();
        anyhow::bail!(
            "failed to upload {} blob(s) to configured file servers{}",
            total_errors,
            detail
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::collect_cids_for_push;
    use crate::HashtreeStore;
    use futures::executor::block_on as sync_block_on;
    use hashtree_core::{HashTree, HashTreeConfig};
    use tempfile::tempdir;

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
}
