use super::*;

const REBUILD_ENV: &str = "HTREE_GIT_REBUILD_FROM_LOCAL";

#[test]
fn local_rebuild_recovers_packed_history_and_preserves_all_refs() {
    check_local_rebuild(None);
}

#[test]
fn local_rebuild_rejects_missing_retained_history_before_upload() {
    check_local_rebuild(Some(false));
}

#[test]
fn local_rebuild_rejects_corrupt_retained_history_before_upload() {
    check_local_rebuild(Some(true));
}

fn check_local_rebuild(corrupt: Option<bool>) {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo, base, master, dev) = create_repo_with_diverged_master_and_dev();
    let _home = HomeGuard::set(home.path());
    let _cwd = CwdGuard::set(repo.path());
    let _git_dir = EnvGuard::clear("GIT_DIR");
    let _verbose = EnvGuard::set("HTREE_VERBOSE", "1");
    let _mode = EnvGuard::set(REBUILD_ENV, "1");
    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    config.server.bind_address = blossom.base_url().trim_start_matches("http://").to_string();
    let keys = nostr::Keys::generate();
    let mut helper = RemoteHelper::new(
        &keys.public_key().to_hex(),
        "test-repo",
        Some(hex::encode(keys.secret_key().to_secret_bytes())),
        None,
        false,
        config,
    )
    .unwrap();
    assert!(git(
        repo.path(),
        &["tag", "-a", "retained", &dev, "-m", "Retained history"]
    )
    .status
    .success());
    let oid = |name: &str| {
        String::from_utf8(git(repo.path(), &["rev-parse", name]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let mut refs = vec![
        ("HEAD", "ref: refs/heads/dev".to_string()),
        ("refs/heads/master", master.clone()),
        ("refs/heads/dev", dev.clone()),
        ("refs/tags/retained", oid("retained")),
        ("refs/tags/tree", oid(&format!("{base}^{{tree}}"))),
        ("refs/tags/blob", oid(&format!("{base}:README.md"))),
        ("refs/stash", dev.clone()),
    ];
    let tips: Vec<_> = refs[1..].iter().map(|(_, sha)| sha.clone()).collect();
    let objects = helper.list_objects_for_shas(&tips, &[]).unwrap();
    for (kind, bytes) in helper.read_git_objects_batch(&objects).unwrap() {
        helper.storage.write_raw_object(kind, &bytes).unwrap();
    }
    for (name, value) in &refs {
        helper.storage.import_ref(name, value).unwrap();
    }
    let mut packs = RemoteHelper::generate_git_pack_checkpoint(&master, None).unwrap();
    let (pack_name, pack_bytes) = packs
        .iter()
        .find(|(name, _)| name.ends_with(".pack"))
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .unwrap();
    packs.extend(RemoteHelper::generate_git_pack_checkpoint(&refs[3].1, None).unwrap());
    helper
        .storage
        .set_pack_checkpoint_files(packs, objects.iter().cloned().collect())
        .unwrap();
    let root = helper.storage.build_tree().unwrap();
    // Both histories live in real packs. Remove an actual chunk, leaving its
    // authenticated directory/index metadata and every ref tip unchanged.
    let root = block_on_result(async {
        let tree =
            HashTree::new(HashTreeConfig::new(helper.storage.store().clone()).with_chunk_size(97));
        let (stash, stash_size) = tree.put(dev.as_bytes()).await?;
        let root = tree
            .set_entry(
                &root,
                &[".git", "refs"],
                "stash",
                &stash,
                stash_size,
                LinkType::File,
            )
            .await?;
        let (pack, size) = tree.put(&pack_bytes).await?;
        let root = tree
            .set_entry(
                &root,
                &[".git", "objects", "pack"],
                &pack_name,
                &pack,
                size,
                LinkType::File,
            )
            .await?;
        let missing = collect_hashes(&tree, &pack, 4)
            .await?
            .into_iter()
            .find(|hash| hash != &pack.hash)
            .unwrap();
        for hash in collect_hashes(&tree, &root, 4).await? {
            if hash != missing {
                blossom.insert_blob(helper.storage.store().get(&hash).await?.unwrap());
            }
        }
        helper.storage.store().delete(&missing).await?;
        Ok(root)
    })
    .unwrap();
    helper.storage.clear().unwrap();
    let read_refs = block_on_result(
        helper
            .nostr
            .fetch_refs_from_hashtree(&hex::encode(root.hash), root.key.as_ref()),
    )
    .unwrap();
    assert_eq!(
        read_refs.get("refs/stash"),
        Some(&dev),
        "direct files in refs/ must survive a real remote read"
    );
    helper.nostr.force_fetch_refs_success_for_test(
        read_refs.clone(),
        Some(hex::encode(root.hash)),
        root.key,
    );
    let advertised = helper.handle_command("list for-push").unwrap().unwrap();
    for (name, value) in &refs[1..] {
        assert!(advertised.contains(&format!("{value} {name}")));
    }
    helper.nostr.force_fetch_refs_success_for_test(
        read_refs,
        Some(hex::encode(root.hash)),
        root.key,
    );
    std::fs::write(repo.path().join("README.md"), "new master contents\n").unwrap();
    assert!(git(repo.path(), &["commit", "-am", "Advance master"])
        .status
        .success());
    refs[1].1 = oid("master");
    if let Some(corrupt) = corrupt {
        let retained_blob = oid("dev:dev-only.txt");
        let path = repo
            .path()
            .join(".git/objects")
            .join(&retained_blob[..2])
            .join(&retained_blob[2..]);
        std::fs::remove_file(&path).unwrap();
        if corrupt {
            let object = crate::git::object::GitObject::new(
                crate::git::object::ObjectType::Blob,
                b"wrong bytes".to_vec(),
            );
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&object.to_loose_format()).unwrap();
            std::fs::write(path, encoder.finish().unwrap()).unwrap();
        }
    }
    helper
        .queue_push("refs/heads/master:refs/heads/master")
        .unwrap();
    let result = helper.execute_push().unwrap().unwrap();
    if corrupt.is_some() {
        assert!(
            result
                .iter()
                .any(|line| line.starts_with("error refs/heads/master ")),
            "{result:?}"
        );
        assert_eq!(blossom.state.lock().unwrap().upload_requests, 0);
        assert_eq!(blossom.state.lock().unwrap().batch_upload_requests, 0);
        return;
    }
    // With no relays this fixture stops at the real metadata publication gate,
    // after the complete tree has passed the real upload and availability gates.
    let rebuilt = helper
        .storage
        .get_root_cid()
        .unwrap()
        .expect("rebuilt root");
    assert!(blossom.has_blob(&rebuilt.hash), "{result:?}");
    let tree = HashTree::new(HashTreeConfig::new(helper.storage.store().clone()));
    block_on_result(async {
        assert!(tree
            .resolve_path(&rebuilt, ".git/objects/pack")
            .await?
            .is_none());
        let packs = tree
            .resolve_path(&rebuilt, ".git/objects/info/packs")
            .await?
            .expect("empty pack inventory");
        assert!(tree.get(&packs, None).await?.unwrap().is_empty());
        for hash in collect_hashes(&tree, &rebuilt, 4).await? {
            assert!(
                blossom.has_blob(&hash),
                "every rebuilt chunk must reach the server"
            );
        }
        Ok(())
    })
    .unwrap();
    let fresh = TempDir::new().unwrap();
    assert!(git(fresh.path(), &["init", "-b", "master"])
        .status
        .success());
    let _fresh_cwd = CwdGuard::set(fresh.path());
    helper
        .nostr
        .cache_root_for_test("test-repo", hex::encode(rebuilt.hash), rebuilt.key);
    for (name, sha) in &refs[1..] {
        helper.queue_fetch(&format!("{sha} {name}")).unwrap();
    }
    helper.execute_fetch().expect("complete rebuilt history");
    hydration::assert_fetched_refs_and_clone(&helper, &rebuilt, &refs, &fresh);
}

#[test]
fn local_rebuild_rejects_invalid_modes_before_writes() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().unwrap();
    let _home = HomeGuard::set(home.path());
    let _mode = EnvGuard::set(REBUILD_ENV, "1");
    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    let mut helper = create_test_helper_with_config(config).unwrap();
    for specs in [
        vec![],
        vec![":refs/heads/master"],
        vec!["+HEAD:refs/heads/master"],
        vec!["HEAD:refs/heads/master", "HEAD:refs/heads/dev"],
        vec!["HEAD:HEAD"],
    ] {
        helper.push_specs.clear();
        for spec in specs {
            helper.queue_push(spec).unwrap();
        }
        let error = helper.execute_push().expect_err("invalid repair mode");
        assert!(error.to_string().contains("local rebuild"), "{error:#}");
    }
    helper.config.blossom.force_upload = true;
    assert!(
        helper.handle_command("list for-push").is_err(),
        "must not hide advertised refs"
    );
    helper.config.blossom.force_upload = false;
    helper.nostr.force_fetch_refs_success_for_test(
        HashMap::from([
            ("HEAD".to_string(), String::new()),
            ("refs/heads/master".to_string(), "1".repeat(40)),
        ]),
        None,
        None,
    );
    assert!(
        helper.handle_command("list for-push").is_err(),
        "empty HEAD must not be advertised for repair"
    );
    for failure in [true, false] {
        if failure {
            helper
                .nostr
                .force_fetch_refs_error_for_test("Failed to download root hash: 404 Not Found");
        } else {
            helper
                .nostr
                .force_fetch_refs_success_for_test(HashMap::new(), None, None);
        }
        assert!(
            helper.handle_command("list for-push").is_err(),
            "repair requires readable existing refs"
        );
    }
    helper.push_specs.clear();
    helper.queue_push("HEAD:refs/heads/master").unwrap();
    helper
        .nostr
        .force_fetch_refs_error_for_test("Failed to download root hash: 404 Not Found");
    assert!(helper
        .execute_push()
        .unwrap_err()
        .to_string()
        .contains("local rebuild"));
    assert!(helper.storage.list_refs().unwrap().is_empty());
    assert_eq!(helper.storage.object_count().unwrap(), 0);
    assert_eq!(blossom.state.lock().unwrap().upload_requests, 0);
    assert_eq!(blossom.state.lock().unwrap().batch_upload_requests, 0);
}

#[test]
fn local_rebuild_keeps_noops_and_rejects_grafted_non_fast_forwards() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo, _, master, dev) = create_repo_with_diverged_master_and_dev();
    let _home = HomeGuard::set(home.path());
    let _cwd = CwdGuard::set(repo.path());
    let _git_dir = EnvGuard::clear("GIT_DIR");
    let _mode = EnvGuard::set(REBUILD_ENV, "1");
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers.clear();
    config.blossom.write_servers.clear();
    let mut helper = create_test_helper_with_config(config).unwrap();
    let refs = HashMap::from([
        ("HEAD".to_string(), "ref: refs/heads/dev".to_string()),
        ("refs/heads/master".to_string(), master.clone()),
        ("refs/heads/dev".to_string(), dev.clone()),
    ]);
    helper
        .nostr
        .force_fetch_refs_success_for_test(refs.clone(), None, None);
    helper
        .queue_push("refs/heads/master:refs/heads/master")
        .unwrap();
    assert_eq!(
        helper.execute_push().unwrap().unwrap(),
        ["ok refs/heads/master", ""]
    );
    assert_eq!(
        helper.storage.object_count().unwrap(),
        0,
        "a no-op must not start rebuilding"
    );
    assert!(helper.storage.get_root_cid().unwrap().is_none());
    // A legacy graft can claim this divergent branch descends from master.
    // The repair must apply the same graph policy to ancestry and closure.
    std::fs::write(
        repo.path().join(".git/info/grafts"),
        format!("{dev} {master}\n"),
    )
    .unwrap();
    assert!(
        git(repo.path(), &["merge-base", "--is-ancestor", &master, &dev])
            .status
            .success()
    );
    helper
        .nostr
        .force_fetch_refs_success_for_test(refs, None, None);
    helper
        .queue_push("refs/heads/dev:refs/heads/master")
        .unwrap();
    let result = helper.execute_push().unwrap().unwrap();
    assert!(result[0].contains("non-fast-forward"), "{result:?}");
    assert_eq!(helper.storage.object_count().unwrap(), 0);
    assert!(helper.storage.get_root_cid().unwrap().is_none());
}

#[test]
fn local_rebuild_rejects_empty_remote_ref_leaf() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().unwrap();
    let _home = HomeGuard::set(home.path());
    let _mode = EnvGuard::set(REBUILD_ENV, "1");
    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers.clear();
    let helper = create_test_helper_with_config(config).unwrap();
    let tree = HashTree::new(HashTreeConfig::new(helper.storage.store().clone()));
    let error = block_on_result(async {
        let (head, _) = tree.put(b"ref: refs/heads/master").await?;
        let (tip, _) = tree.put("1".repeat(40).as_bytes()).await?;
        let (empty, _) = tree.put(b"").await?;
        let heads = tree
            .put_directory(vec![DirEntry::from_cid("master", &tip)])
            .await?;
        let refs = tree
            .put_directory(vec![
                DirEntry::from_cid("heads", &heads).with_link_type(LinkType::Dir),
                DirEntry::from_cid("stash", &empty),
            ])
            .await?;
        let git = tree
            .put_directory(vec![
                DirEntry::from_cid("HEAD", &head),
                DirEntry::from_cid("refs", &refs).with_link_type(LinkType::Dir),
            ])
            .await?;
        let root = tree
            .put_directory(vec![
                DirEntry::from_cid(".git", &git).with_link_type(LinkType::Dir)
            ])
            .await?;
        for hash in collect_hashes(&tree, &root, 4).await? {
            blossom.insert_blob(helper.storage.store().get(&hash).await?.unwrap());
        }
        helper
            .nostr
            .fetch_refs_from_hashtree(&hex::encode(root.hash), root.key.as_ref())
            .await
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("Empty Git ref refs/stash"),
        "{error:#}"
    );
    assert_eq!(blossom.state.lock().unwrap().upload_requests, 0);
}
