use super::*;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use tempfile::TempDir;

const CHILD_TEST: &str = "socialgraph::profile_lock_tests::root_pair_lock_child";

struct LockChild {
    process: Child,
    state: PathBuf,
}

impl LockChild {
    fn spawn(path: &Path, mode: ProfileRootPairLockMode) -> Self {
        let mut child = Self {
            process: Command::new(std::env::current_exe().unwrap())
                .args(["--exact", CHILD_TEST, "--nocapture"])
                .env("HASHTREE_ROOT_PAIR_LOCK_CHILD_PATH", path)
                .env("HASHTREE_ROOT_PAIR_LOCK_CHILD_MODE", format!("{mode:?}"))
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
            state: path.with_extension("state"),
        };
        child.wait_for_state(b"locked");
        child
    }

    fn wait_for_state(&mut self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read(&self.state).ok().as_deref() != Some(expected) {
            assert!(
                self.process.try_wait().unwrap().is_none(),
                "lock child exited early"
            );
            assert!(Instant::now() < deadline, "lock child handshake timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn signal(&mut self) {
        self.process
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"x")
            .unwrap();
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "lock child did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for LockChild {
    fn drop(&mut self) {
        // Reap the fixture even if an exclusion assertion fails.
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[test]
fn root_pair_lock_child() {
    let Some(path) = std::env::var_os("HASHTREE_ROOT_PAIR_LOCK_CHILD_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let mode = match std::env::var("HASHTREE_ROOT_PAIR_LOCK_CHILD_MODE")
        .unwrap()
        .as_str()
    {
        "Shared" => ProfileRootPairLockMode::Shared,
        "Exclusive" => ProfileRootPairLockMode::Exclusive,
        _ => panic!("invalid child lock mode"),
    };
    let guard =
        acquire_profile_root_pair_lock(&path, mode, mode == ProfileRootPairLockMode::Exclusive)
            .unwrap();
    let state = path.with_extension("state");
    std::fs::write(&state, b"locked").unwrap();
    let mut signal = [0];
    std::io::stdin().read_exact(&mut signal).unwrap();
    drop(guard);
    std::fs::write(&state, b"released").unwrap();
    // Stay alive so the parent proves guard release before process termination.
    std::io::stdin().read_exact(&mut signal).unwrap();
}

#[test]
fn cross_process_root_pair_locks_exclude_and_release() {
    for held_mode in [
        ProfileRootPairLockMode::Shared,
        ProfileRootPairLockMode::Exclusive,
    ] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("profile-🔒.lock");
        let contents = b"existing lock file contents";
        std::fs::write(&path, contents).unwrap();
        let mut child = LockChild::spawn(&path, held_mode);
        for requested_mode in [
            ProfileRootPairLockMode::Shared,
            ProfileRootPairLockMode::Exclusive,
        ] {
            let file = try_open_and_lock_profile_root_pair_file(
                &path,
                requested_mode,
                requested_mode == ProfileRootPairLockMode::Exclusive,
            )
            .unwrap();
            assert_eq!(
                file.is_some(),
                held_mode == ProfileRootPairLockMode::Shared && requested_mode == held_mode,
                "OS lock conflict: held {held_mode:?}, requested {requested_mode:?}"
            );
        }
        child.signal();
        child.wait_for_state(b"released");
        assert!(child.process.try_wait().unwrap().is_none());
        let file = try_open_and_lock_profile_root_pair_file(
            &path,
            ProfileRootPairLockMode::Exclusive,
            true,
        )
        .unwrap()
        .expect("dropping the guard must release its OS lock");
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        child.signal();
        assert!(child.wait().success());
    }
}

#[test]
fn process_exit_releases_root_pair_lock() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("profile.lock");
    let mut child = LockChild::spawn(&path, ProfileRootPairLockMode::Exclusive);
    child.process.kill().unwrap();
    assert!(!child.wait().success());
    assert!(
        try_open_and_lock_profile_root_pair_file(&path, ProfileRootPairLockMode::Exclusive, true,)
            .unwrap()
            .is_some(),
        "process termination must release the OS lock"
    );
}

#[test]
fn missing_read_only_root_pair_lock_fails_closed() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("missing.lock");
    let error =
        try_open_and_lock_profile_root_pair_file(&path, ProfileRootPairLockMode::Shared, false)
            .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(
        !path.exists(),
        "read-only acquisition must not create its lock file"
    );
}

#[test]
fn social_graph_startup_initializes_and_releases_root_pair_lock() {
    let _guard = test_lock_blocking();
    let temp = TempDir::new().unwrap();
    let db_dir = temp.path().join("socialgraph");
    let graph = open_fixture(temp.path());
    assert!(read_profile_index_roots(temp.path())
        .unwrap()
        .by_pubkey
        .is_none());
    assert!(
        try_open_and_lock_profile_root_pair_file(
            &db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE),
            ProfileRootPairLockMode::Exclusive,
            true,
        )
        .unwrap()
        .is_some(),
        "startup must not retain the transaction lock"
    );
    drop(graph);
}

fn open_fixture(data_dir: &Path) -> Arc<SocialGraphStore> {
    let db_dir = data_dir.join("socialgraph");
    let local = Arc::new(
        LocalStore::new(db_dir.join("blobs"), &hashtree_config::StorageBackend::Fs).unwrap(),
    );
    open_social_graph_store_at_path_with_storage(
        &db_dir,
        Arc::new(StorageRouter::new(local)),
        Some(32 * 1024 * 1024),
    )
    .unwrap()
}

#[test]
fn profile_commit_replaces_persists_and_reopens() {
    use nostr::{EventBuilder, Keys, Timestamp};

    let _guard = test_lock_blocking();
    let temp = TempDir::new().unwrap();
    let data_dir = temp.path().join("profiles-🔒");
    let db_dir = data_dir.join("socialgraph");
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let mut graph = open_fixture(&data_dir);
    for (i, name) in ["first portable", "second portable"]
        .into_iter()
        .enumerate()
    {
        let event = EventBuilder::new(
            Kind::Metadata,
            serde_json::json!({"display_name": name}).to_string(),
        )
        .custom_created_at(Timestamp::from_secs(i as u64 + 1))
        .sign_with_keys(&keys)
        .unwrap();
        #[cfg(windows)]
        for marker in [
            PROFILE_ROOT_PAIR_COMMIT_FILE,
            PROFILE_PROJECTION_PENDING_FILE,
        ] {
            // A previous crash may leave this ignored cleanup sidecar. The next
            // completed transaction must replace it without replaying its bytes.
            std::fs::write(db_dir.join(format!(".{marker}.deleted")), b"stale cleanup").unwrap();
        }
        ingest_parsed_event_with_storage_class(&graph, &event, EventStorageClass::Public).unwrap();
        let roots = read_profile_index_roots(&data_dir).unwrap();
        assert!(roots.by_pubkey.is_some() && roots.search.is_some());
        for marker in [
            PROFILE_ROOT_PAIR_COMMIT_FILE,
            PROFILE_PROJECTION_PENDING_FILE,
        ] {
            assert!(
                !db_dir.join(marker).exists(),
                "completed marker must be removed"
            );
            assert!(
                !db_dir.join(format!(".{marker}.deleted")).exists(),
                "cleanup must finish"
            );
        }
        // Model interrupted tombstone cleanup: reopen must ignore these bytes.
        #[cfg(windows)]
        std::fs::write(
            db_dir.join(format!(".{PROFILE_PROJECTION_PENDING_FILE}.deleted")),
            b"an already completed projection",
        )
        .unwrap();
        drop(graph);
        graph = open_fixture(&data_dir);
        assert_eq!(read_profile_index_roots(&data_dir).unwrap(), roots);
        assert_eq!(
            graph.latest_profile_event(&pubkey).unwrap().unwrap().id,
            event.id
        );
        let prefix = if i == 0 { "p:first" } else { "p:second" };
        assert_eq!(
            graph
                .profile_search_entries_for_prefix(prefix)
                .unwrap()
                .len(),
            1
        );
        if i == 1 {
            assert!(graph
                .profile_search_entries_for_prefix("p:first")
                .unwrap()
                .is_empty());
        }
    }
}

#[test]
fn durable_profile_file_failures_preserve_recovery_state() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("profile-root.msgpack");
    std::fs::create_dir(&path).unwrap();
    assert!(replace_file_durable(&path, b"pending root", "test root").is_err());
    assert_eq!(
        std::fs::read(temp.path().join(".profile-root.msgpack.pending")).unwrap(),
        b"pending root"
    );
    assert!(path.is_dir());
    assert!(remove_file_durable(&path).is_err());
    assert!(
        path.is_dir(),
        "a failed file deletion must not rename directories"
    );

    #[cfg(windows)]
    {
        let marker = temp.path().join("profile.pending.json");
        std::fs::write(&marker, b"recovery marker").unwrap();
        std::fs::create_dir(temp.path().join(".profile.pending.json.deleted")).unwrap();
        assert!(remove_file_durable(&marker).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), b"recovery marker");
    }
    remove_file_durable(&temp.path().join("absent.marker")).unwrap();
}
