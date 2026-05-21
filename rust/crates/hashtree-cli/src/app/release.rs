use anyhow::{bail, Context, Result};
use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::{
    Config, FetchConfig, Fetcher, HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver,
    NostrToBech32, RootResolver,
};
use hashtree_core::{Cid, HashTree, HashTreeConfig, LinkType, Store};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::blossom::background_blossom_push_incremental_with_store;
use super::resolve::resolve_cid_input;

pub(crate) struct PublishedRelease {
    pub(crate) npub: String,
    pub(crate) tree_name: String,
    pub(crate) version_path: String,
    pub(crate) latest_path: Option<String>,
}

fn parse_release_path(path: &str) -> Result<Vec<String>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        bail!("Version path must not be empty");
    }

    let segments: Vec<String> = trimmed
        .split('/')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect();

    if segments.iter().any(|segment| segment.is_empty()) {
        bail!("Version path must not contain empty segments");
    }

    if segments.last().map(String::as_str) == Some("latest") {
        bail!("Version path must not end with 'latest'");
    }

    Ok(segments)
}

fn latest_path_for(version_segments: &[String]) -> String {
    let mut latest_segments = version_segments.to_vec();
    *latest_segments
        .last_mut()
        .expect("version segments validated as non-empty") = "latest".to_string();
    latest_segments.join("/")
}

async fn ensure_directory_path<S: Store>(
    tree: &HashTree<S>,
    mut root: Cid,
    parent_segments: &[String],
) -> Result<Cid> {
    for depth in 0..parent_segments.len() {
        let segment = &parent_segments[depth];
        let path = parent_segments[..=depth].join("/");

        match tree
            .resolve_path(&root, &path)
            .await
            .with_context(|| format!("Failed to resolve {}", path))?
        {
            Some(existing) => {
                if !tree
                    .is_dir(&existing)
                    .await
                    .with_context(|| format!("Failed to inspect {}", path))?
                {
                    bail!("Release path component is not a directory: {}", path);
                }
            }
            None => {
                let empty_dir = tree
                    .put_directory(Vec::new())
                    .await
                    .context("Failed to create release directory")?;
                let parent_path: Vec<&str> = parent_segments[..depth]
                    .iter()
                    .map(String::as_str)
                    .collect();
                root = tree
                    .set_entry(&root, &parent_path, segment, &empty_dir, 0, LinkType::Dir)
                    .await
                    .with_context(|| format!("Failed to create release directory {}", path))?;
            }
        }
    }

    Ok(root)
}

async fn fetch_existing_directory_chain<S: Store>(
    store: &HashtreeStore,
    fetcher: &Fetcher,
    tree: &HashTree<S>,
    root: &Cid,
    parent_segments: &[String],
) -> Result<()> {
    fetcher
        .fetch_chunk_with_store(store, None, &root.hash)
        .await
        .context("Failed to fetch current release root")?;

    let mut current = root.clone();
    for segment in parent_segments {
        if !tree
            .is_dir(&current)
            .await
            .context("Failed to inspect current release directory")?
        {
            bail!("Release path component is not a directory: {}", segment);
        }

        let entries = tree
            .list_directory(&current)
            .await
            .context("Failed to list current release directory")?;

        let Some(entry) = entries.iter().find(|entry| entry.name == *segment) else {
            break;
        };

        if entry.link_type != LinkType::Dir {
            bail!("Release path component is not a directory: {}", segment);
        }

        let child = Cid {
            hash: entry.hash,
            key: entry.key,
        };
        fetcher
            .fetch_chunk_with_store(store, None, &child.hash)
            .await
            .with_context(|| format!("Failed to fetch directory node for {}", segment))?;
        current = child;
    }

    Ok(())
}

async fn publish_release_root<S: Store>(
    tree: &HashTree<S>,
    current_root: Option<Cid>,
    version_path: &str,
    release_cid: &Cid,
    update_latest: bool,
) -> Result<Cid> {
    let version_segments = parse_release_path(version_path)?;
    let version_name = version_segments
        .last()
        .expect("version segments validated as non-empty")
        .clone();
    let parent_segments = &version_segments[..version_segments.len() - 1];

    let mut root = match current_root {
        Some(root) => root,
        None => tree
            .put_directory(Vec::new())
            .await
            .context("Failed to create initial release root")?,
    };

    root = ensure_directory_path(tree, root, parent_segments)
        .await
        .context("Failed to ensure release directory path")?;

    let parent_path: Vec<&str> = parent_segments.iter().map(String::as_str).collect();
    root = tree
        .set_entry(
            &root,
            &parent_path,
            &version_name,
            release_cid,
            0,
            LinkType::Dir,
        )
        .await
        .with_context(|| format!("Failed to publish release {}", version_path))?;

    if update_latest {
        root = tree
            .set_entry(&root, &parent_path, "latest", release_cid, 0, LinkType::Dir)
            .await
            .context("Failed to update latest release pointer")?;
    }

    Ok(root)
}

pub(crate) async fn publish_release_version(
    data_dir: &Path,
    tree_name: &str,
    version_path: &str,
    cid_input: &str,
    local: bool,
    draft: bool,
) -> Result<PublishedRelease> {
    if tree_name.trim().is_empty() {
        bail!("Release tree name must not be empty");
    }

    let version_segments = parse_release_path(version_path)?;
    let update_latest = !draft;
    let latest_path = if update_latest {
        Some(latest_path_for(&version_segments))
    } else {
        None
    };

    let resolved = resolve_cid_input(cid_input).await?;
    if resolved.path.is_some() {
        bail!("Release CID input must not include a subpath");
    }
    let release_cid = resolved.cid;

    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let fetcher = Fetcher::new(FetchConfig::default());
    fetcher
        .fetch_chunk_with_store(store.as_ref(), None, &release_cid.hash)
        .await
        .context("Failed to fetch release directory root")?;

    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    if !tree
        .is_dir(&release_cid)
        .await
        .context("Failed to inspect release CID")?
    {
        bail!("Release CID must point to a directory");
    }

    let config = Config::load()?;
    let (nsec_str, was_generated) = ensure_keys_string()?;
    let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
    let npub = NostrToBech32::to_bech32(&keys.public_key()).context("Failed to encode npub")?;

    if was_generated {
        println!("Identity: {} (new)", npub);
    }

    let resolver_config = NostrResolverConfig {
        relays: config.nostr.relays.clone(),
        resolve_timeout: Duration::from_secs(5),
        secret_key: Some(keys),
    };
    let resolver = NostrRootResolver::new(resolver_config)
        .await
        .context("Failed to create Nostr resolver")?;

    let nostr_key = format!("{}/{}", npub, tree_name);
    let current_root = resolver
        .resolve(&nostr_key)
        .await
        .with_context(|| format!("Failed to resolve existing release tree {}", nostr_key))?;

    if let Some(root) = current_root.as_ref() {
        println!("Loading existing release path...");
        fetch_existing_directory_chain(
            store.as_ref(),
            &fetcher,
            &tree,
            root,
            &version_segments[..version_segments.len() - 1],
        )
        .await?;
    }

    let new_root = publish_release_root(
        &tree,
        current_root.clone(),
        version_path,
        &release_cid,
        update_latest,
    )
    .await?;

    if !local {
        let mut write_servers = config.blossom.servers.clone();
        write_servers.extend(config.blossom.write_servers.clone());
        if !write_servers.is_empty() {
            println!("Pushing updated release root to file servers...");
            background_blossom_push_incremental_with_store(
                store.clone(),
                new_root.clone(),
                current_root.clone(),
                &write_servers,
            )
            .await
            .context("Failed to push updated release root to file servers")?;
        }
    }

    match resolver.publish(&nostr_key, &new_root).await {
        Ok(true) => {}
        Ok(false) => bail!("Release publish returned false"),
        Err(err) => bail!("Release publish failed: {}", err),
    }

    let _ = resolver.stop().await;

    Ok(PublishedRelease {
        npub,
        tree_name: tree_name.to_string(),
        version_path: version_path.to_string(),
        latest_path,
    })
}

#[cfg(test)]
mod tests {
    use super::{latest_path_for, parse_release_path, publish_release_root};
    use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
    use std::sync::Arc;

    fn make_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());
        (store, tree)
    }

    async fn make_release_dir(tree: &HashTree<MemoryStore>, contents: &[u8]) -> hashtree_core::Cid {
        let (binary_cid, size) = tree.put_file(contents).await.expect("put file");
        tree.put_directory(vec![DirEntry::from_cid(
            "hashtree-x86_64-unknown-linux-musl.tar.gz",
            &binary_cid,
        )
        .with_link_type(LinkType::File)
        .with_size(size)])
            .await
            .expect("put release dir")
    }

    #[test]
    fn parse_release_path_rejects_latest_leaf() {
        let err = parse_release_path("releases/latest").expect_err("latest leaf should fail");
        assert!(err.to_string().contains("must not end with 'latest'"));
    }

    #[test]
    fn latest_path_tracks_version_parent_directory() {
        assert_eq!(
            latest_path_for(&parse_release_path("v0.2.3").unwrap()),
            "latest"
        );
        assert_eq!(
            latest_path_for(&parse_release_path("releases/v0.2.3").unwrap()),
            "releases/latest"
        );
    }

    #[tokio::test]
    async fn publish_release_root_creates_initial_latest_and_version_entries() {
        let (_store, tree) = make_tree();
        let release_cid = make_release_dir(&tree, b"release-one").await;

        let root = publish_release_root(&tree, None, "v0.2.3", &release_cid, true)
            .await
            .expect("publish root");

        let version = tree
            .resolve_path(&root, "v0.2.3")
            .await
            .expect("resolve version")
            .expect("version present");
        let latest = tree
            .resolve_path(&root, "latest")
            .await
            .expect("resolve latest")
            .expect("latest present");

        assert_eq!(version, release_cid);
        assert_eq!(latest, release_cid);
    }

    #[tokio::test]
    async fn publish_release_root_preserves_existing_versions_and_repoints_latest() {
        let (_store, tree) = make_tree();
        let release_v1 = make_release_dir(&tree, b"release-one").await;
        let release_v2 = make_release_dir(&tree, b"release-two").await;

        let root = publish_release_root(&tree, None, "v0.2.2", &release_v1, true)
            .await
            .expect("publish first release");
        let root = publish_release_root(&tree, Some(root), "v0.2.3", &release_v2, true)
            .await
            .expect("publish second release");

        let v1 = tree
            .resolve_path(&root, "v0.2.2")
            .await
            .expect("resolve v1")
            .expect("v1 present");
        let v2 = tree
            .resolve_path(&root, "v0.2.3")
            .await
            .expect("resolve v2")
            .expect("v2 present");
        let latest = tree
            .resolve_path(&root, "latest")
            .await
            .expect("resolve latest")
            .expect("latest present");

        assert_eq!(v1, release_v1);
        assert_eq!(v2, release_v2);
        assert_eq!(latest, release_v2);
    }

    #[tokio::test]
    async fn publish_release_root_creates_nested_parent_directories() {
        let (_store, tree) = make_tree();
        let release_cid = make_release_dir(&tree, b"release-three").await;

        let root = publish_release_root(&tree, None, "releases/v0.2.3", &release_cid, true)
            .await
            .expect("publish nested release");

        let version = tree
            .resolve_path(&root, "releases/v0.2.3")
            .await
            .expect("resolve nested version")
            .expect("nested version present");
        let latest = tree
            .resolve_path(&root, "releases/latest")
            .await
            .expect("resolve nested latest")
            .expect("nested latest present");

        assert_eq!(version, release_cid);
        assert_eq!(latest, release_cid);
    }

    #[tokio::test]
    async fn publish_release_root_draft_does_not_repoint_latest() {
        let (_store, tree) = make_tree();
        let stable_release = make_release_dir(&tree, b"stable-release").await;
        let draft_release = make_release_dir(&tree, b"draft-release").await;

        let root = publish_release_root(&tree, None, "v0.2.3", &stable_release, true)
            .await
            .expect("publish stable release");
        let root = publish_release_root(&tree, Some(root), "v0.2.4-rc.1", &draft_release, false)
            .await
            .expect("publish draft release");

        let draft = tree
            .resolve_path(&root, "v0.2.4-rc.1")
            .await
            .expect("resolve draft")
            .expect("draft present");
        let latest = tree
            .resolve_path(&root, "latest")
            .await
            .expect("resolve latest")
            .expect("latest present");

        assert_eq!(draft, draft_release);
        assert_eq!(latest, stable_release);
    }
}
