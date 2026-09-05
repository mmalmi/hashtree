use super::*;

#[test]
fn push_error_status_is_one_protocol_line() {
    let status = crate::helper::push::push_error_status(
        "refs/heads/master",
        &anyhow::anyhow!("missing objects:\r\n  first\n  second\r"),
    );
    assert!(status.starts_with("error refs/heads/master "));
    assert!(!status.contains(['\r', '\n']), "{status:?}");
    assert!(status.contains("first") && status.contains("second"));
}

#[test]
fn cached_root_hydration_recovers_local_objects_and_preserves_history() {
    check_cached_root_hydration(None);
}

#[test]
fn cached_root_hydration_rejects_unavailable_local_objects() {
    check_cached_root_hydration(Some(false));
}

#[test]
fn cached_root_hydration_rejects_corrupt_local_objects() {
    check_cached_root_hydration(Some(true));
}

fn check_cached_root_hydration(corrupt_local: Option<bool>) {
    let _env_lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (home, repo, base_sha, master_sha, dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());
    let _git_dir_guard = EnvGuard::clear("GIT_DIR");
    // Use the existing shorter progress interval to avoid reporter shutdown delays.
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
        ("refs/heads/master", master_sha.clone()),
        ("refs/heads/dev", dev_sha),
        ("refs/tags/retained", base_sha),
    ];
    let tips: Vec<_> = refs[1..].iter().map(|(_, sha)| sha.clone()).collect();
    let objects = helper.list_objects_for_shas(&tips, &[]).expect("history");
    let content = helper
        .read_git_objects_batch(&objects)
        .expect("local objects");
    for (kind, bytes) in content {
        helper
            .storage
            .write_raw_object(kind, &bytes)
            .expect("seed remote");
    }
    for (name, sha) in &refs {
        helper.storage.import_ref(name, sha).expect("seed ref");
    }
    let root = helper.storage.build_tree().expect("remote root");
    helper
        .nostr
        .cache_root_for_test("test-repo", hex::encode(root.hash), root.key);
    let missing_oid =
        String::from_utf8(git(repo.path(), &["rev-parse", "dev:dev-only.txt"]).stdout)
            .unwrap()
            .trim()
            .to_string();
    block_on_result(async {
        let (tree, locations, _, _) = helper
            .collect_git_object_locations_async(&hex::encode(root.hash), root.key.as_ref())
            .await?;
        let location = locations
            .iter()
            .find(|entry| entry.oid == missing_oid)
            .unwrap();
        let hashes = collect_hashes(&tree, &location.cid, 4).await?;
        for hash in hashes {
            helper.storage.store().delete(&hash).await?;
        }
        Ok(())
    })
    .expect("remove remote object chunks");
    helper.storage.clear().expect("clear import state");

    if let Some(corrupt) = corrupt_local {
        let path = repo
            .path()
            .join(".git/objects")
            .join(&missing_oid[..2])
            .join(&missing_oid[2..]);
        if corrupt {
            // Valid zlib/object framing with a different content hash catches trusting cat-file's OID.
            let replacement = crate::git::object::GitObject::new(
                crate::git::object::ObjectType::Blob,
                b"wrong content".to_vec(),
            );
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&replacement.to_loose_format()).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::write(path, encoder.finish().unwrap()).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
        let error = helper
            .fetch_all_git_objects(&hex::encode(root.hash))
            .expect_err("missing or corrupt local object must not be omitted");
        assert!(
            error.to_string().contains("required git objects"),
            "{error:#}"
        );
        assert_eq!(helper.storage.object_count().unwrap(), 0);
        return;
    }

    assert!(git(repo.path(), &["repack", "-ad"]).status.success());
    let recovered = helper
        .fetch_all_git_objects(&hex::encode(root.hash))
        .expect("recover unavailable remote object from local Git pack");
    assert_eq!(recovered.len(), objects.len());
    for (oid, content) in recovered {
        helper
            .storage
            .import_compressed_object(&oid, content)
            .expect("validated object");
    }
    for (name, sha) in &refs {
        helper.storage.import_ref(name, sha).expect("retained ref");
    }
    let (tree, _) = helper.build_cached_fetch_tree().expect("cached tree");
    let rebuilt = helper
        .storage
        .build_tree_with_base_objects(Some(&tree), Some(&root), None)
        .expect("merge recovered objects into the existing root");

    let fresh = TempDir::new().expect("fresh repository");
    assert!(git(fresh.path(), &["init", "-b", "master"])
        .status
        .success());
    let _fresh_cwd = CwdGuard::set(fresh.path());
    helper
        .nostr
        .cache_root_for_test("test-repo", hex::encode(rebuilt.hash), rebuilt.key);
    helper
        .fetch_git_objects_to_local_git(&hex::encode(rebuilt.hash))
        .expect("fresh fetch of rebuilt root");
    let tree = HashTree::new(HashTreeConfig::new(helper.storage.store().clone()));
    for (name, expected) in &refs {
        let bytes = block_on_result(async {
            let cid = tree
                .resolve_path(&rebuilt, &format!(".git/{name}"))
                .await?
                .expect("retained ref path");
            Ok(tree.get(&cid, None).await?.expect("retained ref contents"))
        })
        .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap().trim(), expected);
        let args = if *name == "HEAD" {
            vec!["symbolic-ref", "HEAD", "refs/heads/master"]
        } else {
            vec!["update-ref", name, expected]
        };
        assert!(git(fresh.path(), &args).status.success());
    }
    let cloned = TempDir::new().expect("clone destination");
    let clone = git(
        fresh.path(),
        &[
            "clone",
            "--mirror",
            "--no-local",
            ".",
            cloned.path().to_str().unwrap(),
        ],
    );
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );
    let fsck = git(cloned.path(), &["fsck", "--full"]);
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert_eq!(
        git(fresh.path(), &["show-ref"]).stdout,
        git(cloned.path(), &["show-ref"]).stdout
    );
}
