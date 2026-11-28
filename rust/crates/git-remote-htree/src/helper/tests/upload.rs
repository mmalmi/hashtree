use super::*;
use crate::git::object::{GitObject, ObjectType};
use std::io::Read;

#[test]
fn force_upload_repairs_unchanged_tip_from_local_history() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo, _, master, dev) = create_repo_with_diverged_master_and_dev();
    let _home = HomeGuard::set(home.path());
    let _cwd = CwdGuard::set(repo.path());
    let _git_dir = EnvGuard::clear("GIT_DIR");
    let _mode = EnvGuard::clear("HTREE_GIT_REBUILD_FROM_LOCAL");
    let data = TempDir::new().unwrap();
    let _data = EnvGuard::set("HTREE_DATA_DIR", data.path().to_str().unwrap());
    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    config.blossom.force_upload = true;
    config.server.bind_address = blossom.base_url().trim_start_matches("http://").to_string();
    let keys = nostr::Keys::generate();
    let mut helper = RemoteHelper::new(
        &keys.public_key().to_hex(),
        "test-repo",
        Some(hex::encode(keys.secret_key().to_secret_bytes())),
        None,
        false,
        config.clone(),
    )
    .unwrap();
    let refs = HashMap::from([
        ("refs/heads/master".to_string(), master.clone()),
        ("refs/heads/dev".to_string(), dev.clone()),
        ("refs/tags/retained".to_string(), dev.clone()),
    ]);
    let expected = helper
        .list_objects_for_shas(&[master.clone(), dev], &[])
        .unwrap()
        .into_iter()
        .collect::<HashSet<_>>();
    assert_eq!(
        helper.handle_command("list for-push").unwrap().unwrap(),
        vec![String::new()]
    );
    // Existing refs are readable, but this publisher and server have none of
    // their objects. Re-uploading the same tip must reconstruct full history.
    helper
        .nostr
        .force_fetch_refs_success_for_test(refs.clone(), None, None);
    helper
        .queue_push("refs/heads/master:refs/heads/master")
        .unwrap();
    let result = helper.execute_push().unwrap().unwrap();
    // No relay is configured: exercise the real upload before the metadata gate.
    assert!(
        blossom.get_upload_request_count() + blossom.get_batch_upload_request_count() > 0,
        "{result:?}"
    );
    assert_eq!(blossom.get_upload_check_request_count(), 0);
    let root = helper.storage.get_root_cid().unwrap().unwrap();

    let fresh_data = TempDir::new().unwrap();
    let _fresh_data = EnvGuard::set("HTREE_DATA_DIR", fresh_data.path().to_str().unwrap());
    let fresh = create_test_helper_with_config(config).unwrap();
    let fetched =
        block_on_result(fresh.fetch_git_objects_async(&hex::encode(root.hash), root.key.as_ref()))
            .unwrap();
    assert_eq!(
        fetched
            .into_iter()
            .map(|(oid, _)| oid)
            .collect::<HashSet<_>>(),
        expected
    );
    let fetched_refs = block_on_result(
        fresh
            .nostr
            .fetch_refs_from_hashtree(&hex::encode(root.hash), root.key.as_ref()),
    )
    .unwrap();
    for (name, value) in refs {
        assert_eq!(fetched_refs.get(&name), Some(&value));
    }
}

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
    let helper = create_signed_test_helper(config.clone());
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
            let plaintext = hashtree_core::decrypt_object(&encrypted, cid.key.as_ref().unwrap())?;
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

    let result = helper.push_to_file_servers(&hex::encode(root.hash), root.key.as_ref());
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

#[test]
fn multi_ref_push_batches_ancestor_branches_and_tags_into_one_upload() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo, base_sha, master_sha, dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home = HomeGuard::set(home.path());
    let _cwd = CwdGuard::set(repo.path());
    let _git_dir = EnvGuard::clear("GIT_DIR");
    let _mode = EnvGuard::clear("HTREE_GIT_REBUILD_FROM_LOCAL");
    let data = TempDir::new().unwrap();
    let _data = EnvGuard::set("HTREE_DATA_DIR", data.path().to_str().unwrap());

    // Create an ancestor branch pointing to `base_sha` and two tags at `master_sha`.
    for args in [
        vec!["branch", "ancestor", &base_sha],
        vec!["tag", "v0.1", &master_sha],
        vec!["tag", "v0.2", &master_sha],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    config.server.bind_address = blossom.base_url().trim_start_matches("http://").to_string();
    let keys = nostr::Keys::generate();
    let helper = RemoteHelper::new(
        &keys.public_key().to_hex(),
        "test-repo",
        Some(hex::encode(keys.secret_key().to_secret_bytes())),
        None,
        false,
        config.clone(),
    )
    .unwrap();

    // Seed initial remote tree containing `refs/heads/master`.
    let master_objects = helper
        .list_objects_for_shas(std::slice::from_ref(&master_sha), &[])
        .unwrap();
    let master_contents = helper.read_git_objects_batch(&master_objects).unwrap();
    helper
        .storage
        .write_raw_objects_batch(&master_contents)
        .unwrap();
    helper
        .storage
        .import_ref("refs/heads/master", &master_sha)
        .unwrap();
    helper
        .storage
        .import_ref("HEAD", "ref: refs/heads/master")
        .unwrap();
    let initial_root = helper.storage.build_tree().unwrap();
    let seed_upload =
        helper.push_to_file_servers(&hex::encode(initial_root.hash), initial_root.key.as_ref());
    assert!(seed_upload.failed.is_empty(), "{:?}", seed_upload.failed);

    // Fresh helper pushing 4 refs together (ancestor branch + diverged dev + 2 tags).
    let mut pusher = RemoteHelper::new(
        &keys.public_key().to_hex(),
        "test-repo",
        Some(hex::encode(keys.secret_key().to_secret_bytes())),
        None,
        false,
        config.clone(),
    )
    .unwrap();
    let remote_refs = HashMap::from([
        ("HEAD".to_string(), "ref: refs/heads/master".to_string()),
        ("refs/heads/master".to_string(), master_sha.clone()),
    ]);
    pusher.nostr.force_fetch_refs_success_for_test(
        remote_refs,
        Some(hex::encode(initial_root.hash)),
        initial_root.key,
    );
    pusher
        .queue_push("refs/heads/ancestor:refs/heads/ancestor")
        .unwrap();
    pusher.queue_push("refs/heads/dev:refs/heads/dev").unwrap();
    pusher.queue_push("refs/tags/v0.1:refs/tags/v0.1").unwrap();
    pusher.queue_push("refs/tags/v0.2:refs/tags/v0.2").unwrap();

    let batch_before = blossom.get_batch_upload_request_count();
    let _ = pusher.execute_push().unwrap();
    let batch_after = blossom.get_batch_upload_request_count();
    assert_eq!(
        batch_after - batch_before,
        1,
        "4-ref push should upload in a single batched tree publish, not 4 separate uploads"
    );

    let new_root = pusher.storage.get_root_cid().unwrap().unwrap();
    let fresh_data = TempDir::new().unwrap();
    let _fresh_data = EnvGuard::set("HTREE_DATA_DIR", fresh_data.path().to_str().unwrap());
    let fresh = create_test_helper_with_config(config).unwrap();
    let fetched_refs = block_on_result(
        fresh
            .nostr
            .fetch_refs_from_hashtree(&hex::encode(new_root.hash), new_root.key.as_ref()),
    )
    .unwrap();
    assert_eq!(fetched_refs.get("refs/heads/master"), Some(&master_sha));
    assert_eq!(fetched_refs.get("refs/heads/ancestor"), Some(&base_sha));
    assert_eq!(fetched_refs.get("refs/heads/dev"), Some(&dev_sha));
    assert_eq!(fetched_refs.get("refs/tags/v0.1"), Some(&master_sha));
    assert_eq!(fetched_refs.get("refs/tags/v0.2"), Some(&master_sha));
}
