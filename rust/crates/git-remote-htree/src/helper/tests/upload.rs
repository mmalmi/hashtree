use super::*;
use crate::git::object::{GitObject, ObjectType};
use std::io::Read;

#[test]
fn git_storage_upload_includes_chunked_objects_and_multilevel_descendants() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().unwrap();
    let _home = HomeGuard::set(home.path());
    let source_data = TempDir::new().unwrap();
    let _data = EnvGuard::set("HTREE_DATA_DIR", source_data.path().to_str().unwrap());
    let _progress = EnvGuard::set("HTREE_VERBOSE", "1");
    let blossom = CountingBlossomServer::new();
    write_test_config(home.path(), blossom.base_url(), true);
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    let helper = create_test_helper_with_config(config.clone()).unwrap();
    let mut expected = HashMap::new();
    // Incompressible content exercises the real GitStorage writer: a file below
    // the core default chunk size, and a file with multiple chunk-tree levels.
    for size in [128 * 1024, 12 * 1024 * 1024] {
        let mut bytes = Vec::with_capacity(size);
        for counter in 0..size / 32 {
            bytes.extend_from_slice(&Sha256::digest(counter.to_le_bytes()));
        }
        let oid = helper
            .storage
            .write_raw_object(ObjectType::Blob, &bytes)
            .unwrap();
        expected.insert(oid.to_hex(), bytes);
    }
    let root = helper.storage.build_tree().unwrap();
    let hashes = block_on_result(async {
        let tree = HashTree::new(HashTreeConfig::new(helper.storage.store().clone()));
        for (oid, bytes) in &expected {
            let path = format!(".git/objects/{}/{}", &oid[..2], &oid[2..]);
            let cid = tree.resolve_path(&root, &path).await?.unwrap();
            let encrypted = helper.storage.store().get(&cid.hash).await?.unwrap();
            let plaintext = hashtree_core::decrypt_chk(&encrypted, cid.key.as_ref().unwrap())?;
            let node = hashtree_core::decode_tree_node(&plaintext)?;
            assert_eq!(node.node_type, LinkType::File);
            assert_eq!(
                node.links
                    .iter()
                    .any(|link| link.link_type == LinkType::File),
                bytes.len() > hashtree_core::DEFAULT_CHUNK_SIZE,
                "the larger Git object must exercise multiple encrypted tree levels"
            );
        }
        Ok(collect_hashes(&tree, &root, 4).await?)
    })
    .unwrap();

    let result = helper.push_to_file_servers_with_diff(
        &hex::encode(root.hash),
        root.key.as_ref(),
        None,
        None,
        false,
    );
    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let missing = hashes.iter().filter(|hash| !blossom.has_blob(hash)).count();
    assert_eq!(missing, 0, "upload omitted descendants of real Git objects");

    // A different, empty local store must read every byte over HTTP. The source
    // store remains intact, so accidental shared-cache reuse cannot hide misses.
    let fresh_data = TempDir::new().unwrap();
    let _fresh_data = EnvGuard::set("HTREE_DATA_DIR", fresh_data.path().to_str().unwrap());
    let fresh = create_test_helper_with_config(config).unwrap();
    for hash in &hashes {
        assert!(fresh.storage.store().get_sync(hash).unwrap().is_none());
    }
    let fetched =
        block_on_result(fresh.fetch_git_objects_async(&hex::encode(root.hash), root.key.as_ref()))
            .unwrap();
    assert_eq!(fetched.len(), expected.len());
    for (oid, compressed) in fetched {
        let mut loose = Vec::new();
        flate2::read::ZlibDecoder::new(compressed.as_slice())
            .read_to_end(&mut loose)
            .unwrap();
        let object = GitObject::from_loose_format(&loose).unwrap();
        assert_eq!(object.obj_type, ObjectType::Blob);
        assert_eq!(object.id().to_hex(), oid);
        assert_eq!(object.content, expected.remove(&oid).unwrap());
    }
    assert!(expected.is_empty());
    assert!(blossom.get_request_count() > 0);
}
