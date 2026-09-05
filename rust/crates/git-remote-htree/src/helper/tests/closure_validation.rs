use super::*;

fn with_history(check: impl FnOnce(&mut RemoteHelper, &std::path::Path, &str, &str)) {
    let _env_lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (home, repo, base, tip) = create_repo_with_large_base_and_small_increment();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());
    let _git_dir_guard = EnvGuard::clear("GIT_DIR");
    let mut config = Config::default();
    config.nostr.relays.clear();
    config.blossom.read_servers.clear();
    config.blossom.write_servers.clear();
    config.server.bind_address = "127.0.0.1:0".to_string();
    let mut helper = create_test_helper_with_config(config).unwrap();
    helper
        .queue_fetch(&format!("{tip} refs/heads/master"))
        .unwrap();
    check(&mut helper, repo.path(), &base, &tip);
}

fn loose_path(repo: &std::path::Path, oid: &str) -> std::path::PathBuf {
    repo.join(".git/objects").join(&oid[..2]).join(&oid[2..])
}

#[test]
fn requested_history_requires_valid_nonempty_object_ids() {
    with_history(|helper, _, _, _| {
        helper
            .verify_requested_git_object_closure()
            .expect("complete history");
        helper.fetch_specs.clear();
        assert!(helper.verify_requested_git_object_closure().is_err());
        helper
            .queue_fetch("not-an-object refs/heads/master")
            .unwrap();
        assert!(helper.verify_requested_git_object_closure().is_err());
    });
}

#[test]
fn requested_history_rejects_shallow_and_grafted_ancestors() {
    with_history(|helper, repo, base, tip| {
        std::fs::remove_file(loose_path(repo, base)).unwrap();
        std::fs::write(repo.join(".git/shallow"), format!("{tip}\n")).unwrap();
        let error = helper.verify_requested_git_object_closure().unwrap_err();
        assert!(error.to_string().contains("shallow"), "{error:#}");
        std::fs::remove_file(repo.join(".git/shallow")).unwrap();
        std::fs::write(repo.join(".git/info/grafts"), format!("{tip}\n")).unwrap();
        let error = helper.verify_requested_git_object_closure().unwrap_err();
        assert!(error.to_string().contains("rev-list"), "{error:#}");
    });
}

#[test]
fn requested_history_rejects_wrong_content_under_a_valid_object_id() {
    with_history(|helper, repo, _, tip| {
        let oid =
            String::from_utf8(git(repo, &["rev-parse", &format!("{tip}:increment.txt")]).stdout)
                .unwrap()
                .trim()
                .to_string();
        let object = crate::git::object::GitObject::new(
            crate::git::object::ObjectType::Blob,
            b"wrong content".to_vec(),
        );
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&object.to_loose_format()).unwrap();
        let path = loose_path(repo, &oid);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(path, encoder.finish().unwrap()).unwrap();
        let error = helper.verify_requested_git_object_closure().unwrap_err();
        assert!(error.to_string().contains("fsck"), "{error:#}");
    });
}

#[test]
fn requested_history_rejects_promised_missing_objects() {
    with_history(|helper, repo, _, tip| {
        let missing =
            String::from_utf8(git(repo, &["rev-parse", &format!("{tip}:base-00.txt")]).stdout)
                .unwrap()
                .trim()
                .to_string();
        let objects = helper.list_objects_to_push(tip, &[]).unwrap();
        let pack_prefix = repo.join(".git/objects/pack/pack");
        let mut child = Command::new("git")
            .arg("pack-objects")
            .arg(&pack_prefix)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            for oid in objects.iter().filter(|oid| **oid != missing) {
                writeln!(stdin, "{oid}").unwrap();
            }
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let pack_hash = String::from_utf8(output.stdout).unwrap();
        std::fs::write(
            pack_prefix.with_file_name(format!("pack-{}.promisor", pack_hash.trim())),
            b"",
        )
        .unwrap();
        for oid in &objects {
            std::fs::remove_file(loose_path(repo, oid)).unwrap();
        }
        for (key, value) in [
            ("core.repositoryFormatVersion", "1"),
            ("extensions.partialClone", "fixture"),
            ("remote.fixture.promisor", "true"),
            ("remote.fixture.partialclonefilter", "blob:none"),
            ("remote.fixture.url", "./absent-local-fixture"),
        ] {
            assert!(git(repo, &["config", key, value]).status.success());
        }
        let fsck = Command::new("git")
            .args(["fsck", "--full", "--no-reflogs", "--no-dangling", tip])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .unwrap();
        assert!(
            fsck.status.success(),
            "Git accepts promisor omissions: {}",
            String::from_utf8_lossy(&fsck.stderr)
        );
        let error = helper.verify_requested_git_object_closure().unwrap_err();
        assert!(error.to_string().contains("rev-list"), "{error:#}");
    });
}
