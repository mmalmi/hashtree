use super::{
    block_on_result, create_signed_test_helper, write_test_config, Config, CountingBlossomServer,
    EnvGuard, HomeGuard, TempDir, ENV_LOCK,
};
use hashtree_core::{sha256, Cid, DirEntry, Hash, HashTree, HashTreeConfig, LinkType, Store};
use std::{collections::HashSet, sync::Arc};

async fn chunked_listing<S: Store>(
    source: &Arc<S>,
    child: &Cid,
    name: &str,
    len: u64,
) -> (Cid, HashSet<Hash>) {
    let tree = HashTree::new(HashTreeConfig::new(source.clone()).public());
    let directory = tree
        .put_directory(vec![DirEntry::from_cid(name, child).with_size(len)])
        .await
        .unwrap();
    let serialized = source.get(&directory.hash).await.unwrap().unwrap();
    let pieces = HashTree::new(
        HashTreeConfig::new(source.clone())
            .public()
            .with_chunk_size(16),
    );
    let (cid, _) = pieces.put(&serialized).await.unwrap();
    let manifest = pieces.get_node(&cid).await.unwrap().unwrap();
    assert!(manifest.is_file() && manifest.links.len() > 1);
    let mut blocks: HashSet<_> = manifest.links.into_iter().map(|l| l.hash).collect();
    blocks.insert(cid.hash);
    (cid, blocks)
}

fn verify_push(root_is_chunked: bool) {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().unwrap();
    let _home = HomeGuard::set(home.path());
    let data = TempDir::new().unwrap();
    let _data = EnvGuard::set("HTREE_DATA_DIR", data.path().to_str().unwrap());
    let writer = CountingBlossomServer::new();
    write_test_config(home.path(), writer.base_url(), false);
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![writer.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    let helper = create_signed_test_helper(config);
    let source = helper.storage.store().clone();
    let (root, required) = block_on_result(async {
        let tree = HashTree::new(HashTreeConfig::new(source.clone()).public());
        let (leaf, len) = tree
            .put(b"only reachable from the assembled directory")
            .await?;
        let (directory, mut required) = chunked_listing(&source, &leaf, "leaf", len).await;
        required.insert(leaf.hash);
        if root_is_chunked {
            assert_eq!(tree.resolve_path(&directory, "leaf").await?, Some(leaf));
            return Ok((directory, required));
        }
        let absent = Cid::public(sha256(b"not part of the repository"));
        let (literal, physical_file) = chunked_listing(&source, &absent, "unrelated", 23).await;
        let literal_bytes = tree.get(&literal, None).await?.unwrap();
        assert!(hashtree_core::decode_tree_node(&literal_bytes)?.is_dir());
        let root = tree
            .put_directory(vec![
                DirEntry::from_cid("folder", &directory).with_link_type(LinkType::Dir),
                DirEntry::from_cid("document", &literal)
                    .with_link_type(LinkType::File)
                    .with_size(literal_bytes.len() as u64),
            ])
            .await?;
        assert_eq!(tree.resolve_path(&root, "folder/leaf").await?, Some(leaf));
        required.insert(root.hash);
        required.extend(physical_file);
        Ok((root, required))
    })
    .unwrap();
    let result = helper.push_to_file_servers(&hex::encode(root.hash), root.key.as_ref());
    assert_eq!(
        result.succeeded,
        vec![writer.base_url().to_owned()],
        "{:?}",
        result.failed
    );
    for hash in required {
        assert!(
            writer.has_blob(&hash),
            "Git publication omitted a required block"
        );
    }
}

#[test]
fn git_publication_preserves_chunked_directories_and_literal_files() {
    verify_push(false);
}

#[test]
fn git_publication_treats_its_root_as_a_directory() {
    verify_push(true);
}
