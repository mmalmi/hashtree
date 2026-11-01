use super::*;
use git_remote_htree::git::{object::ObjectType, refs::Ref, storage::GitStorage};
use hashtree_core::{Cid, HashTree, HashTreeConfig};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::FromRawFd,
};

async fn terminal(args: &[&str], directory: &Path) -> (std::process::ExitStatus, String) {
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_htree"));
    let binaries = Path::new(env!("CARGO_BIN_EXE_git-remote-htree"))
        .parent()
        .unwrap();
    let path = std::env::join_paths(std::iter::once(binaries.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    command
        .args(args)
        .env("HTREE_CONFIG_DIR", directory)
        .env("HTREE_DATA_DIR", directory.join("data"))
        .env("PATH", path)
        .env_remove("NOSTR_RELAYS")
        .env_remove("HTREE_LOCAL_DAEMON_ONLY")
        .env("NOSTR_PREFER_LOCAL", "0")
        .env("TERM", "xterm")
        .env("GIT_TERMINAL_PROMPT", "1")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    drop(command);
    // The terminal line discipline keeps the answer until the picker reads it.
    master.write_all(b"1\n").unwrap();
    let transcript = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&chunk[..count]),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("terminal read: {error}"),
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let status = tokio::time::timeout(Duration::from_secs(50), child.wait())
        .await
        .expect("CLI deadline")
        .unwrap();
    (status, transcript.await.unwrap())
}

fn announcement(keys: &Keys, name: &str, cid: &Cid, git: bool) -> Event {
    let mut tags = vec![
        Tag::identifier(name),
        Tag::parse(["l", "hashtree"]).unwrap(),
        Tag::parse(["hash", &hex::encode(cid.hash)]).unwrap(),
    ];
    if git {
        tags.push(Tag::parse(["l", "git"]).unwrap());
    }
    if let Some(key) = cid.key {
        tags.push(Tag::parse(["key", &hex::encode(key)]).unwrap());
    }
    event(keys, Kind::Custom(30064), "", tags, 1)
}

#[tokio::test]
async fn terminal_selection_downloads_content_and_clones_with_pinned_origin() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("source");
    let (storage, root, commit) = tokio::task::spawn_blocking(move || {
        let storage = GitStorage::open(source).unwrap();
        let blob = storage.write_raw_object(ObjectType::Blob, b"hello from the chosen publisher\n").unwrap();
        let mut tree = b"100644 README.txt\0".to_vec(); tree.extend(hex::decode(blob.to_hex()).unwrap());
        let tree = storage.write_raw_object(ObjectType::Tree, &tree).unwrap();
        let commit = storage.write_raw_object(ObjectType::Commit, format!("tree {}\nauthor Test <test@example.invalid> 1 +0000\ncommitter Test <test@example.invalid> 1 +0000\n\nFixture\n", tree.to_hex()).as_bytes()).unwrap();
        storage.write_ref("refs/heads/main", &Ref::Direct(commit)).unwrap();
        storage.write_ref("HEAD", &Ref::Symbolic("refs/heads/main".into())).unwrap();
        let root = storage.build_tree().unwrap();
        (storage, root, commit)
    }).await.unwrap();
    let tree = HashTree::new(HashTreeConfig::new(storage.store().clone()).public());
    let (note, _) = tree.put(b"hello from a named tree").await.unwrap();
    let author = Keys::generate();
    let state = Fixture {
        events: vec![event(
            &author,
            Kind::Metadata,
            r#"{"name":"alice"}"#,
            vec![],
            1,
        )],
        late: vec![
            announcement(&author, "project", &root, true),
            announcement(&author, "notes", &note, false),
        ],
        delivered: Arc::new(AtomicUsize::new(0)),
        blobs: Some(storage.store().clone()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/", get(relay))
                .route("/:hash", get(blob))
                .with_state(state),
        )
        .await
        .unwrap();
    });
    config(dir.path(), &format!("ws://{address}"), &author);
    let config_path = dir.path().join("config.toml");
    let settings = std::fs::read_to_string(&config_path).unwrap().replace(
        "read_servers = []",
        &format!("read_servers = [\"http://{address}\"]"),
    );
    std::fs::write(&config_path, settings).unwrap();
    let destination = dir.path().join("clone");
    let (status, transcript) = terminal(
        &["clone", "project", destination.to_str().unwrap()],
        dir.path(),
    )
    .await;
    assert!(status.success(), "{transcript}");
    assert!(transcript.contains("Choose 1-1"), "{transcript}");
    assert_eq!(
        std::fs::read_to_string(destination.join("README.txt")).unwrap(),
        "hello from the chosen publisher\n"
    );
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(args)
            .output()
            .unwrap()
    };
    let origin = git(&["remote", "get-url", "origin"]);
    assert_eq!(
        String::from_utf8(origin.stdout).unwrap().trim(),
        format!(
            "htree://{}/project",
            author.public_key().to_bech32().unwrap()
        )
    );
    let head = git(&["rev-parse", "HEAD"]);
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim(),
        commit.to_hex()
    );
    let output = dir.path().join("note.txt");
    let (status, transcript) = terminal(
        &["get", "notes", "-o", output.to_str().unwrap()],
        dir.path(),
    )
    .await;
    assert!(status.success(), "{transcript}");
    assert!(transcript.contains("Choose 1-1"), "{transcript}");
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "hello from a named tree"
    );
    server.abort();
}
