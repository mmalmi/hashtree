use super::*;

#[test]
fn missing_pack_fetch_recovers_complete_loose_history() {
    check_missing_pack_fetch(false, false);
}

#[test]
fn missing_pack_fetch_accepts_verified_cached_history() {
    check_missing_pack_fetch(true, false);
}

#[test]
fn missing_pack_fetch_rejects_incomplete_loose_history() {
    check_missing_pack_fetch(false, true);
}

#[test]
fn missing_pack_fetch_rejects_incomplete_cached_history() {
    check_missing_pack_fetch(true, true);
}

fn check_missing_pack_fetch(cached: bool, packed_only_blob: bool) {
    let _env_lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (home, repo, base_sha, master_sha, dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());
    let _git_dir_guard = EnvGuard::clear("GIT_DIR");
    let _progress_guard = EnvGuard::set("HTREE_VERBOSE", "1");
    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers.clear();
    config.server.bind_address = blossom.base_url().trim_start_matches("http://").to_string();
    let mut helper = create_test_helper_with_config(config).expect("helper");
    let refs = [
        ("HEAD", "ref: refs/heads/master".to_string()),
        ("refs/heads/master", master_sha),
        ("refs/heads/dev", dev_sha.clone()),
        ("refs/tags/retained", base_sha),
    ];
    let tips: Vec<_> = refs[1..].iter().map(|(_, sha)| sha.clone()).collect();
    let objects = helper.list_objects_for_shas(&tips, &[]).unwrap();
    for (kind, content) in helper.read_git_objects_batch(&objects).unwrap() {
        helper.storage.write_raw_object(kind, &content).unwrap();
    }
    for (name, sha) in &refs {
        helper.storage.import_ref(name, sha).unwrap();
    }
    let blob_oid = String::from_utf8(git(repo.path(), &["rev-parse", "dev:dev-only.txt"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let packs = RemoteHelper::generate_git_pack_checkpoint(&dev_sha, None).unwrap();
    let (pack_name, pack_bytes) = packs
        .iter()
        .find(|(name, _)| name.ends_with(".pack"))
        .unwrap();
    let covered = if packed_only_blob {
        HashSet::from([blob_oid.clone()])
    } else {
        HashSet::new()
    };
    helper
        .storage
        .set_pack_checkpoint_files(packs.clone(), covered)
        .unwrap();
    let root = helper.storage.build_tree().unwrap();
    let root = block_on_result(async {
        let tree =
            HashTree::new(HashTreeConfig::new(helper.storage.store().clone()).with_chunk_size(97));
        let (pack_cid, size) = tree.put(pack_bytes).await?;
        let root = tree
            .set_entry(
                &root,
                &[".git", "objects", "pack"],
                pack_name,
                &pack_cid,
                size,
                LinkType::File,
            )
            .await?;
        let leaf = collect_hashes(&tree, &pack_cid, 4)
            .await?
            .into_iter()
            .find(|hash| hash != &pack_cid.hash)
            .expect("real pack has multiple chunks");
        helper.storage.store().delete(&leaf).await?;
        Ok(root)
    })
    .unwrap();
    helper
        .nostr
        .cache_root_for_test("test-repo", hex::encode(root.hash), root.key);

    let fresh = TempDir::new().unwrap();
    assert!(git(fresh.path(), &["init", "-b", "master"])
        .status
        .success());
    if cached {
        for oid in &objects {
            if packed_only_blob && oid == &blob_oid {
                continue;
            }
            let content = helper.read_verified_compressed_git_object(oid).unwrap();
            RemoteHelper::write_git_object_to_dir(&fresh.path().join(".git"), oid, &content)
                .unwrap();
        }
    }
    let _fresh_cwd = CwdGuard::set(fresh.path());
    for (name, sha) in &refs[1..] {
        helper.queue_fetch(&format!("{sha} {name}")).unwrap();
    }
    // Repeated requests must not change which complete histories are verified.
    helper
        .queue_fetch(&format!("{} refs/heads/dev", tips[1]))
        .unwrap();
    let fetched = helper.execute_fetch();
    if packed_only_blob {
        let error = fetched.expect_err("tip presence cannot substitute for packed-only history");
        assert!(error.to_string().contains("pack"), "{error:#}");
        assert_eq!(
            helper
                .git_batch_check_objects(tips.iter().map(String::as_str))
                .unwrap()
                .len(),
            tips.len()
        );
        assert!(!git(fresh.path(), &["cat-file", "-e", &blob_oid])
            .status
            .success());
        assert_eq!(
            helper.fetch_specs.len(),
            4,
            "failed fetch must not report success"
        );
        return;
    }
    fetched.expect("missing pack may be recovered from complete, verified loose history");
    assert!(helper.fetch_specs.is_empty());
    super::hydration::assert_fetched_refs_and_clone(&helper, &root, &refs, &fresh);
}
