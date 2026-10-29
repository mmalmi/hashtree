use super::*;
use std::sync::atomic::Ordering;

#[test]
fn packed_base_repair_batches_more_than_512_objects_and_preserves_diverged_refs() {
    check_repair(None);
}

#[test]
fn packed_base_repair_rejects_missing_retained_object() {
    check_repair(Some(false));
}

#[test]
fn packed_base_repair_rejects_corrupt_retained_object() {
    check_repair(Some(true));
}

fn check_repair(corrupt: Option<bool>) {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (home, repo, base, master, _) = create_repo_with_diverged_master_and_dev();
    let _home = HomeGuard::set(home.path());
    let _cwd = CwdGuard::set(repo.path());
    let _git_dir = EnvGuard::clear("GIT_DIR");
    let _verbose = EnvGuard::set("HTREE_VERBOSE", "1");
    assert!(git(repo.path(), &["checkout", "dev"]).status.success());
    // A retained branch differs from the delta base in more objects than the
    // old one-object retry limit. None of these blobs belong to the pushed tip.
    let count = if corrupt.is_none() { 520 } else { 4 };
    for i in 0..count {
        std::fs::write(
            repo.path().join(format!("retained-{i}.txt")),
            format!("content {i}\n"),
        )
        .unwrap();
    }
    assert!(git(repo.path(), &["add", "."]).status.success());
    assert!(git(repo.path(), &["commit", "-m", "Retained files"])
        .status
        .success());
    let oid = |name: &str| {
        String::from_utf8(git(repo.path(), &["rev-parse", name]).stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let dev = oid("dev");
    assert!(git(
        repo.path(),
        &["tag", "-a", "retained", "dev", "-m", "Retained tag"]
    )
    .status
    .success());
    let refs = vec![
        ("HEAD", "ref: refs/heads/dev".to_string()),
        ("refs/heads/master", master.clone()),
        ("refs/heads/dev", dev.clone()),
        ("refs/tags/retained", oid("retained")),
        ("refs/tags/blob", oid("dev:retained-0.txt")),
        ("refs/tags/tree", oid("dev^{tree}")),
    ];
    let blossom = CountingBlossomServer::new();
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers = vec![blossom.base_url().to_string()];
    config.blossom.write_servers.clear();
    let mut helper = create_test_helper_with_config(config).expect("helper");
    let tips = refs[1..]
        .iter()
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>();
    let objects = helper.list_objects_for_shas(&tips, &[]).unwrap();
    for (kind, bytes) in helper.read_git_objects_batch(&objects).unwrap() {
        helper.storage.write_raw_object(kind, &bytes).unwrap();
    }
    for (name, value) in &refs {
        helper.storage.import_ref(name, value).unwrap();
    }
    // The cached root's working tree belongs to master, while its dev history
    // is packed. Repairing a tag push must also retain dev's complete closure.
    helper
        .storage
        .import_ref("HEAD", "ref: refs/heads/master")
        .unwrap();
    let mut packs = RemoteHelper::generate_git_pack_checkpoint(&master, None).unwrap();
    packs.extend(RemoteHelper::generate_git_pack_checkpoint(&dev, None).unwrap());
    packs.extend(RemoteHelper::generate_git_pack_checkpoint(&refs[3].1, None).unwrap());
    helper
        .storage
        .set_pack_checkpoint_files(packs, objects.iter().cloned().collect())
        .unwrap();
    let root = helper.storage.build_tree().unwrap();
    let tree = HashTree::new(HashTreeConfig::new(helper.storage.store().clone()));
    let old_packs =
        block_on_result(async { Ok(tree.resolve_path(&root, ".git/objects/pack").await?) })
            .unwrap()
            .unwrap();
    helper.storage.clear().unwrap();
    helper
        .storage
        .set_pack_checkpoint_files(Default::default(), objects.iter().cloned().collect())
        .unwrap();
    helper
        .nostr
        .cache_root_for_test("test-repo", hex::encode(root.hash), root.key);
    for (name, value) in &refs {
        helper.storage.import_ref(name, value).unwrap();
    }
    helper.storage.import_ref("refs/tags/new", &master).unwrap();
    let mut expected_refs = refs.clone();
    expected_refs.push(("refs/tags/new", master.clone()));
    let trees = RemoteHelper::current_tree_tree_object_ids(&dev).unwrap();
    let seed = objects
        .iter()
        .filter(|oid| trees.contains(*oid) || tips.contains(oid))
        .cloned()
        .collect::<Vec<_>>();
    for (kind, bytes) in helper.read_git_objects_batch(&seed).unwrap() {
        helper.storage.write_raw_object(kind, &bytes).unwrap();
    }
    let first = helper
        .build_tree_with_cached_remote_root("Fixture initial merge", Some(&master))
        .unwrap_err();
    assert!(first.to_string().contains("Object not found"), "{first:#}");
    let missing = oid("dev:retained-0.txt");
    if let Some(corrupt) = corrupt {
        let path = repo
            .path()
            .join(".git/objects")
            .join(&missing[..2])
            .join(&missing[2..]);
        std::fs::remove_file(&path).unwrap();
        if corrupt {
            let object = crate::git::object::GitObject::new(
                crate::git::object::ObjectType::Blob,
                b"wrong content".to_vec(),
            );
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&object.to_loose_format()).unwrap();
            std::fs::write(path, encoder.finish().unwrap()).unwrap();
        }
    } else {
        assert!(git(repo.path(), &["repack", "-ad"]).status.success());
    }
    let result = helper.repair_delta_tree_build(
        &master,
        "refs/tags/new",
        &master,
        "Fixture incomplete merge",
        first.to_string(),
    );
    assert!(
        helper.cached_root_build_attempts.load(Ordering::Relaxed) <= 2,
        "repair must not restart the entire merge for each missing object"
    );
    if corrupt.is_some() {
        assert!(
            result.is_err(),
            "missing or corrupt retained history must fail closed"
        );
        assert_eq!(
            helper.storage.list_refs().unwrap(),
            expected_refs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        );
        return;
    }
    let repaired = result.expect("packed-base repair");
    let new_packs =
        block_on_result(async { Ok(tree.resolve_path(&repaired, ".git/objects/pack").await?) })
            .unwrap()
            .unwrap();
    assert_eq!(old_packs, new_packs, "inherited packs must be retained");
    let fresh = TempDir::new().unwrap();
    assert!(git(fresh.path(), &["init", "-b", "master"])
        .status
        .success());
    let _fresh_cwd = CwdGuard::set(fresh.path());
    helper
        .nostr
        .cache_root_for_test("test-repo", hex::encode(repaired.hash), repaired.key);
    for (name, value) in &expected_refs[1..] {
        helper.queue_fetch(&format!("{value} {name}")).unwrap();
    }
    helper
        .execute_fetch()
        .expect("fresh fetch of complete repaired history");
    hydration::assert_fetched_refs_and_clone(&helper, &repaired, &expected_refs, &fresh);
    assert!(git(
        fresh.path(),
        &[
            "rev-list",
            "--objects",
            "--missing=error",
            &master,
            &dev,
            &base
        ]
    )
    .status
    .success());
}
