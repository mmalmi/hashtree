use anyhow::{bail, Context, Result};
use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::{Config, FetchConfig, Fetcher, HashtreeStore, NostrKeys, NostrToBech32};
use hashtree_core::{Cid, HashTree, HashTreeConfig, LinkType, Store, TreeEntry};
use std::path::Path;
use std::sync::Arc;

use super::blossom::background_blossom_push_incremental_with_store;
use super::resolve::resolve_cid_input;

mod transport;
use transport::ReleasePublisher;

pub(crate) struct PublishedRelease {
    pub(crate) npub: String,
    pub(crate) tree_name: String,
    pub(crate) version_path: String,
    pub(crate) latest_path: Option<String>,
    pub(crate) draft_path: Option<String>,
    pub(crate) root: Cid,
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

    if matches!(
        segments.last().map(String::as_str),
        Some("latest" | "draft")
    ) {
        bail!("Version path must not end with 'latest' or 'draft'");
    }

    Ok(segments)
}

fn sibling_path_for(version_segments: &[String], sibling_name: &str) -> String {
    let mut sibling_segments = version_segments.to_vec();
    *sibling_segments
        .last_mut()
        .expect("version segments validated as non-empty") = sibling_name.to_string();
    sibling_segments.join("/")
}

fn latest_path_for(version_segments: &[String]) -> String {
    sibling_path_for(version_segments, "latest")
}

fn draft_path_for(version_segments: &[String]) -> String {
    sibling_path_for(version_segments, "draft")
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
    require_history: bool,
) -> Result<()> {
    fetcher
        .fetch_chunk_with_store(store, &root.hash)
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

        let entries = if require_history {
            release_directory_entries(tree, &current).await?
        } else {
            tree.list_directory(&current)
                .await
                .context("Failed to list current release directory")?
        };

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
            .fetch_chunk_with_store(store, &child.hash)
            .await
            .with_context(|| format!("Failed to fetch directory node for {}", segment))?;
        current = child;
    }

    if require_history {
        release_directory_entries(tree, &current).await?;
    }
    Ok(())
}

async fn release_directory_entries<S: Store>(
    tree: &HashTree<S>,
    root: &Cid,
) -> Result<Vec<TreeEntry>> {
    if !tree
        .is_dir(root)
        .await
        .context("Failed to inspect existing release history")?
    {
        bail!("Existing release history is missing or not a directory");
    }
    tree.list_directory_required(root)
        .await
        .context("Failed to read existing release history")
}

fn release_base_root(
    current_root: Option<Cid>,
    expected_root: Option<&Cid>,
) -> Result<Option<Cid>> {
    match (current_root, expected_root) {
        (Some(observed), Some(expected)) if &observed != expected => {
            bail!("Observed release tree differs from --expected-root; refusing to overwrite release history")
        }
        (_, Some(expected)) => Ok(Some(expected.clone())),
        (observed, None) => Ok(observed),
    }
}

async fn publish_release_root<S: Store>(
    tree: &HashTree<S>,
    current_root: Option<Cid>,
    version_path: &str,
    release_cid: &Cid,
    publish_as_draft: bool,
    expected_root: Option<&Cid>,
) -> Result<Cid> {
    let current_root = release_base_root(current_root, expected_root)?;
    if let Some(root) = expected_root {
        release_directory_entries(tree, root).await?;
    }
    let version_segments = parse_release_path(version_path)?;
    let version_name = version_segments
        .last()
        .expect("version segments validated as non-empty")
        .clone();
    let parent_segments = &version_segments[..version_segments.len() - 1];

    let mut root = match current_root {
        Some(root) if expected_root.is_some() || root.hash != release_cid.hash => root,
        None => tree
            .put_directory(Vec::new())
            .await
            .context("Failed to create initial release root")?,
        Some(_) => tree
            .put_directory(Vec::new())
            .await
            .context("Failed to create replacement release root")?,
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

    let pointer_name = if publish_as_draft { "draft" } else { "latest" };
    root = tree
        .set_entry(
            &root,
            &parent_path,
            pointer_name,
            release_cid,
            0,
            LinkType::Dir,
        )
        .await
        .with_context(|| format!("Failed to update {} release pointer", pointer_name))?;

    Ok(root)
}

pub(crate) async fn publish_release_version(
    data_dir: &Path,
    tree_name: &str,
    version_path: &str,
    cid_input: &str,
    local: bool,
    draft: bool,
    expected_root: Option<Cid>,
) -> Result<PublishedRelease> {
    if tree_name.trim().is_empty() {
        bail!("Release tree name must not be empty");
    }

    let version_segments = parse_release_path(version_path)?;
    let latest_path = (!draft).then(|| latest_path_for(&version_segments));
    let draft_path = draft.then(|| draft_path_for(&version_segments));

    let resolved = resolve_cid_input(cid_input).await?;
    if resolved.path.is_some() {
        bail!("Release CID input must not include a subpath");
    }
    let release_cid = resolved.cid;

    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let fetcher = Fetcher::new(FetchConfig::default());
    fetcher
        .fetch_chunk_with_store(store.as_ref(), &release_cid.hash)
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

    let publisher = ReleasePublisher::connect(&config, keys).await?;
    let nostr_key = format!("{}/{}", npub, tree_name);
    let (current_root, latest_created_at) = publisher
        .resolve(&nostr_key, expected_root.is_some())
        .await
        .with_context(|| format!("Failed to resolve existing release tree {}", nostr_key))?;
    let current_root = release_base_root(current_root, expected_root.as_ref())?;

    if let Some(root) = current_root.as_ref() {
        println!("Loading existing release path...");
        fetch_existing_directory_chain(
            store.as_ref(),
            &fetcher,
            &tree,
            root,
            &version_segments[..version_segments.len() - 1],
            expected_root.is_some(),
        )
        .await?;
    }

    let new_root = publish_release_root(
        &tree,
        current_root.clone(),
        version_path,
        &release_cid,
        draft,
        expected_root.as_ref(),
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

    publisher
        .publish(&nostr_key, &new_root, latest_created_at)
        .await?;

    Ok(PublishedRelease {
        npub,
        tree_name: tree_name.to_string(),
        version_path: version_path.to_string(),
        latest_path,
        draft_path,
        root: new_root,
    })
}

#[cfg(test)]
mod tests {
    use super::{draft_path_for, latest_path_for, parse_release_path, publish_release_root};
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

    #[tokio::test]
    async fn publish_release_root_wraps_existing_release_directory() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store));
        let asset = tree.put_blob(b"asset").await.expect("asset");
        let release = tree
            .put_directory(vec![DirEntry::new("release.json", asset).with_size(5)])
            .await
            .expect("release directory");

        let root = publish_release_root(
            &tree,
            Some(release.clone()),
            "v0.2.69",
            &release,
            false,
            None,
        )
        .await
        .expect("publish release root");

        assert_ne!(root.hash, release.hash);
        let version = tree
            .resolve_path(&root, "v0.2.69")
            .await
            .expect("resolve version")
            .expect("version entry");
        assert_eq!(version.hash, release.hash);
        assert_eq!(
            tree.resolve_path(&root, "latest")
                .await
                .expect("resolve latest")
                .expect("latest entry")
                .hash,
            release.hash
        );
        let root_entries = tree.list_directory(&root).await.expect("list root");
        assert_eq!(root_entries.len(), 2);
        assert!(root_entries
            .iter()
            .all(|entry| entry.link_type == LinkType::Dir));
    }

    #[test]
    fn parse_release_path_rejects_pointer_leaves() {
        let err = parse_release_path("releases/latest").expect_err("latest leaf should fail");
        assert!(err
            .to_string()
            .contains("must not end with 'latest' or 'draft'"));

        let err = parse_release_path("releases/draft").expect_err("draft leaf should fail");
        assert!(err
            .to_string()
            .contains("must not end with 'latest' or 'draft'"));
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

    #[test]
    fn draft_path_tracks_version_parent_directory() {
        assert_eq!(
            draft_path_for(&parse_release_path("v0.2.4-rc.1").unwrap()),
            "draft"
        );
        assert_eq!(
            draft_path_for(&parse_release_path("releases/v0.2.4-rc.1").unwrap()),
            "releases/draft"
        );
    }

    #[tokio::test]
    async fn publish_release_root_creates_initial_latest_and_version_entries() {
        let (_store, tree) = make_tree();
        let release_cid = make_release_dir(&tree, b"release-one").await;

        let root = publish_release_root(&tree, None, "v0.2.3", &release_cid, false, None)
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

        let root = publish_release_root(&tree, None, "v0.2.2", &release_v1, false, None)
            .await
            .expect("publish first release");
        let root = publish_release_root(&tree, Some(root), "v0.2.3", &release_v2, false, None)
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
    async fn expected_root_preserves_history_when_no_head_is_observed() {
        let (_store, tree) = make_tree();
        let first = make_release_dir(&tree, b"first").await;
        let second = make_release_dir(&tree, b"second").await;
        let previous = publish_release_root(&tree, None, "v1", &first, false, None)
            .await
            .unwrap();
        let updated = publish_release_root(&tree, None, "v2", &second, false, Some(&previous))
            .await
            .unwrap();
        assert_eq!(
            tree.resolve_path(&updated, "v1").await.unwrap(),
            Some(first)
        );
        assert_eq!(
            tree.resolve_path(&updated, "v2").await.unwrap(),
            Some(second.clone())
        );
        assert_eq!(
            tree.resolve_path(&updated, "latest").await.unwrap(),
            Some(second)
        );
    }

    #[tokio::test]
    async fn expected_root_rejects_a_conflicting_observed_head() {
        let (_store, tree) = make_tree();
        let release = make_release_dir(&tree, b"first").await;
        let observed = publish_release_root(&tree, None, "v1", &release, false, None)
            .await
            .unwrap();
        let expected = hashtree_core::Cid::public([7; 32]);
        let error = publish_release_root(
            &tree,
            Some(observed),
            "v2",
            &release,
            false,
            Some(&expected),
        )
        .await
        .expect_err("conflicting release history must not be overwritten");
        assert!(error.to_string().contains("differs from --expected-root"));
    }

    #[tokio::test]
    async fn expected_root_missing_data_cannot_create_a_fresh_tree() {
        let (_store, tree) = make_tree();
        let release = make_release_dir(&tree, b"first").await;
        let missing = hashtree_core::Cid::public([7; 32]);
        publish_release_root(&tree, None, "v2", &release, false, Some(&missing))
            .await
            .expect_err("unavailable previous root must not become a new tree");
    }

    #[tokio::test]
    async fn expected_root_rejects_a_chunked_file_as_release_history() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store).with_chunk_size(4).public());
        let (file, _) = tree.put_file(&[0xab; 32]).await.unwrap();
        let release = make_release_dir(&tree, b"first").await;
        publish_release_root(&tree, None, "v2", &release, false, Some(&file))
            .await
            .expect_err("ordinary chunked files are not release history");
    }

    #[tokio::test]
    async fn expected_root_is_preserved_even_when_it_is_the_new_release_cid() {
        let (_store, tree) = make_tree();
        let first = make_release_dir(&tree, b"first").await;
        let previous = publish_release_root(&tree, None, "v1", &first, false, None)
            .await
            .unwrap();
        let updated = publish_release_root(
            &tree,
            Some(previous.clone()),
            "v2",
            &previous,
            false,
            Some(&previous),
        )
        .await
        .unwrap();
        assert_eq!(
            tree.resolve_path(&updated, "v1").await.unwrap(),
            Some(first)
        );
        assert_eq!(
            tree.resolve_path(&updated, "v2").await.unwrap(),
            Some(previous)
        );
    }

    #[tokio::test]
    async fn publish_release_root_creates_nested_parent_directories() {
        let (_store, tree) = make_tree();
        let release_cid = make_release_dir(&tree, b"release-three").await;

        let root = publish_release_root(&tree, None, "releases/v0.2.3", &release_cid, false, None)
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
    async fn publish_release_root_draft_repoints_draft_not_latest() {
        let (_store, tree) = make_tree();
        let stable_release = make_release_dir(&tree, b"stable-release").await;
        let draft_release = make_release_dir(&tree, b"draft-release").await;

        let root = publish_release_root(&tree, None, "v0.2.3", &stable_release, false, None)
            .await
            .expect("publish stable release");
        let root =
            publish_release_root(&tree, Some(root), "v0.2.4-rc.1", &draft_release, true, None)
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
        let draft_pointer = tree
            .resolve_path(&root, "draft")
            .await
            .expect("resolve draft pointer")
            .expect("draft pointer present");

        assert_eq!(draft, draft_release);
        assert_eq!(draft_pointer, draft_release);
        assert_eq!(latest, stable_release);
    }
}
