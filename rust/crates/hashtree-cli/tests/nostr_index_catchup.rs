use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use futures::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind, Timestamp};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

struct Relay {
    url: String,
    events: Arc<Mutex<Vec<Event>>>,
    fail_author: Arc<Mutex<Option<String>>>,
    omit_eose: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    event_page_cap: Arc<AtomicUsize>,
    count_filters: Arc<Mutex<Vec<Value>>>,
    capacity_filters: Arc<Mutex<Vec<Value>>>,
    count_supported: Arc<AtomicBool>,
    count_silent: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(events: Vec<Event>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let events = Arc::new(Mutex::new(events));
        let fail_author = Arc::new(Mutex::new(None));
        let omit_eose = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let event_page_cap = Arc::new(AtomicUsize::new(usize::MAX));
        let count_filters = Arc::new(Mutex::new(Vec::new()));
        let capacity_filters = Arc::new(Mutex::new(Vec::new()));
        let count_supported = Arc::new(AtomicBool::new(true));
        let count_silent = Arc::new(AtomicBool::new(false));
        let state = (
            events.clone(),
            fail_author.clone(),
            omit_eose.clone(),
            connections.clone(),
            event_page_cap.clone(),
            count_filters.clone(),
            capacity_filters.clone(),
            count_supported.clone(),
            count_silent.clone(),
        );
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (
                    events,
                    fail_author,
                    omit_eose,
                    connections,
                    event_page_cap,
                    count_filters,
                    capacity_filters,
                    count_supported,
                    count_silent,
                ) = state.clone();
                connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let mut socket = accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let message: Value = serde_json::from_str(&text).unwrap();
                        if message[0] != "REQ" && message[0] != "COUNT" {
                            continue;
                        }
                        let subscription = &message[1];
                        let filter = &message[2];
                        let author = filter["authors"][0].as_str();
                        if author.is_some_and(|author| {
                            fail_author.lock().unwrap().as_deref() == Some(author)
                        }) {
                            let _ = socket.close(None).await;
                            return;
                        }
                        let mut matched = events
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|event| {
                                author.is_none_or(|author| event.pubkey.to_hex() == author)
                                    && filter["kinds"].as_array().unwrap().iter().any(|kind| {
                                        kind.as_u64() == Some(u64::from(event.kind.as_u16()))
                                    })
                                    && event.created_at.as_secs()
                                        >= filter["since"].as_u64().unwrap()
                                    && event.created_at.as_secs()
                                        <= filter["until"].as_u64().unwrap()
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        if message[0] == "COUNT" {
                            // The count describes the complete exact filter before
                            // the simulated relay's lower EVENT result cap.
                            count_filters.lock().unwrap().push(filter.clone());
                            if count_silent.load(Ordering::Relaxed) {
                                continue;
                            }
                            let response = if count_supported.load(Ordering::Relaxed) {
                                json!(["COUNT", subscription, {"count": matched.len()}])
                            } else {
                                json!(["CLOSED", subscription, "unsupported: COUNT unavailable"])
                            };
                            if socket
                                .send(Message::Text(response.to_string()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        if author.is_none() {
                            capacity_filters.lock().unwrap().push(filter.clone());
                        }
                        matched.sort_by_key(|event| std::cmp::Reverse(event.created_at));
                        matched.truncate(
                            (filter["limit"].as_u64().unwrap() as usize)
                                .min(event_page_cap.load(Ordering::Relaxed)),
                        );
                        for event in matched {
                            if socket
                                .send(Message::Text(
                                    json!(["EVENT", subscription, event]).to_string(),
                                ))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        if !omit_eose.load(Ordering::Relaxed)
                            && socket
                                .send(Message::Text(json!(["EOSE", subscription]).to_string()))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            url,
            events,
            fail_author,
            omit_eose,
            connections,
            event_page_cap,
            count_filters,
            capacity_filters,
            count_supported,
            count_silent,
            task,
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn event(keys: &Keys, at: u64, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from_secs(at))
        .sign_with_keys(keys)
        .unwrap()
}

fn command(temp: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_htree"));
    command
        .args(["--data-dir"])
        .arg(temp.path().join("data"))
        .env("HTREE_CONFIG_DIR", temp.path().join("config"))
        .env("HTREE_DATA_DIR", temp.path().join("data"))
        .env("HOME", temp.path())
        .env("TOKIO_WORKER_THREADS", "2")
        .env_remove("NOSTR_SECRET_KEY")
        .env_remove("NOSTR_PRIVATE_KEY")
        .env_remove("NOSTR_KEY");
    command
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("{err}: {}", String::from_utf8_lossy(&output.stdout)))
}

fn import(temp: &TempDir, event: &Event) -> String {
    import_events(temp, &[event.clone()])
}

fn import_events(temp: &TempDir, events: &[Event]) -> String {
    import_events_with_external(temp, events, false)
}

fn import_events_with_external(temp: &TempDir, events: &[Event], external: bool) -> String {
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/config.toml"),
        "[storage]\nmax_size_gb = 1\nevict_orphans = false\n",
    )
    .unwrap();
    let path = temp.path().join("events.json");
    std::fs::write(&path, serde_json::to_vec(events).unwrap()).unwrap();
    let mut cmd = command(temp);
    if external {
        cmd.env("HTREE_LMDB_EXTERNAL_BLOB_MIN_BYTES", "1024")
            .env("HTREE_LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES", "1048576");
    }
    success(
        cmd.args(["nostr-index", "import", "--events"])
            .arg(path)
            .output()
            .unwrap(),
    )["root"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn catchup(temp: &TempDir, root: &str, relay: &Relay) -> Command {
    catchup_with_floor(temp, root, relay, 0)
}

fn catchup_with_floor(temp: &TempDir, root: &str, relay: &Relay, floor: u64) -> Command {
    let mut command = command(temp);
    command
        .args(["nostr-index", "catch-up", "--root", root, "--authors-file"])
        .arg(temp.path().join("authors.txt"))
        .args([
            "--min-free-bytes",
            &floor.to_string(),
            "--since",
            "10",
            "--relay",
            &relay.url,
            "--kind",
            "1",
            "--kind",
            "5",
            "--page-size",
            "4",
            "--fetch-timeout-secs",
            "1",
        ]);
    command
}

fn checkpoint(temp: &TempDir) -> Value {
    serde_json::from_slice(
        &std::fs::read(temp.path().join("data/nostr-index/catchup-state.json")).unwrap(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_restarts_failed_author_then_continues_time_without_losing_history() {
    let temp = TempDir::new().unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let old = event(&alice, 1, "archive");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!(
            "{}\n{}\n",
            alice.public_key().to_hex(),
            bob.public_key().to_hex()
        ),
    )
    .unwrap();
    let original_crawl = temp.path().join("data/nostr-index/crawl-state.json");
    std::fs::write(&original_crawl, "original completed crawl").unwrap();
    let relay = Relay::new(vec![
        event(&alice, 20, "alice new"),
        event(&bob, 30, "bob new"),
    ])
    .await;
    let first = success(
        catchup(&temp, &root, &relay)
            .args([
                "--until",
                "100",
                "--max-authors-per-run",
                "1",
                "--read-cache-mib",
                "0",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(first["next_author"], 1);
    assert_eq!(first["complete"], false);
    assert_eq!(
        relay.connections.load(Ordering::Relaxed),
        1,
        "pages reuse one connection"
    );
    let durable = checkpoint(&temp);
    *relay.fail_author.lock().unwrap() = Some(bob.public_key().to_hex());
    let failed = catchup(&temp, &root, &relay).output().unwrap();
    assert!(!failed.status.success());
    assert_eq!(
        checkpoint(&temp),
        durable,
        "failed author must not advance root or coverage"
    );
    *relay.fail_author.lock().unwrap() = None;
    // Enabling the cache on resume is operational tuning, not a policy change.
    let completed_output = catchup(&temp, &root, &relay).output().unwrap();
    assert!(String::from_utf8_lossy(&completed_output.stderr).contains("read_cache_hits="));
    let completed = success(completed_output);
    assert_eq!(completed["complete"], true);
    assert_eq!(
        completed["pass_until"], 100,
        "restart retains captured until"
    );
    relay.events.lock().unwrap().extend([
        event(&alice, 100, "inclusive overlap"),
        event(&alice, 90, "late relay arrival"),
        event(&bob, 150, "next pass"),
    ]);
    let next = success(
        catchup(&temp, &root, &relay)
            .args(["--until", "200"])
            .output()
            .unwrap(),
    );
    assert_eq!(next["pass_since"], 10);
    let current = success(
        command(&temp)
            .args([
                "nostr-index",
                "query",
                "--root",
                next["root"].as_str().unwrap(),
                "--filter",
                "{}",
                "--limit",
                "100",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(current["count"], 6);
    let prior = success(
        command(&temp)
            .args([
                "nostr-index",
                "query",
                "--root",
                &root,
                "--filter",
                "{}",
                "--limit",
                "100",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(prior["count"], 1, "old root must remain readable");
    assert_eq!(
        std::fs::read_to_string(original_crawl).unwrap(),
        "original completed crawl"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_rejects_missing_eose_and_unreadable_base_without_advancing() {
    let temp = TempDir::new().unwrap();
    let keys = Keys::generate();
    let root = import(&temp, &event(&keys, 1, "old"));
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n", keys.public_key().to_hex()),
    )
    .unwrap();
    let relay = Relay::new(vec![event(&keys, 20, "uncommitted")]).await;
    let missing = catchup(&temp, &"00".repeat(32), &relay)
        .args(["--until", "100"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(!temp
        .path()
        .join("data/nostr-index/catchup-state.json")
        .exists());
    assert_eq!(relay.connections.load(Ordering::Relaxed), 0);
    relay.omit_eose.store(true, Ordering::Relaxed);
    let failed = catchup(&temp, &root, &relay)
        .args(["--until", "100"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("timeout before EOSE"));
    assert_eq!(checkpoint(&temp)["next_author"], 0);
    assert_eq!(checkpoint(&temp)["root"], root);
}

fn query_id_count(temp: &TempDir, root: &str, ids: &[String]) -> u64 {
    success(
        command(temp)
            .args([
                "nostr-index",
                "query",
                "--root",
                root,
                "--filter",
                &json!({"ids": ids}).to_string(),
                "--limit",
                "100",
            ])
            .output()
            .unwrap(),
    )["count"]
        .as_u64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exact_count_completes_same_second_pair_and_retains_history() {
    let temp = TempDir::new().unwrap();
    let keys = Keys::generate();
    let old = event(&keys, 1, "retained archive");
    let first = event(&keys, 20, "first tied post");
    let second = event(&keys, 20, "second tied post");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n", keys.public_key()),
    )
    .unwrap();
    let wrong_kind = EventBuilder::new(Kind::Reaction, "+")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&keys)
        .unwrap();
    let relay = Relay::new(vec![
        old.clone(),
        first.clone(),
        second.clone(),
        wrong_kind,
        event(&keys, 200, "outside pass"),
    ])
    .await;
    relay.event_page_cap.store(2, Ordering::Relaxed);
    let completed = success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100"])
            .output()
            .unwrap(),
    );
    assert_eq!(completed["next_author"], 1);
    assert_eq!(completed["complete"], true);
    assert_eq!(completed["events_received"], 2);
    assert_eq!(completed["pass_until"], 100);
    assert_eq!(
        *relay.count_filters.lock().unwrap(),
        vec![
            json!({"authors": [keys.public_key().to_hex()], "kinds": [1, 5], "since": 20, "until": 20})
        ],
        "exact COUNT must be bounded to this author/second/kinds, without a limit",
    );
    assert_eq!(
        relay.connections.load(Ordering::Relaxed),
        1,
        "COUNT should reuse the event socket"
    );
    assert_eq!(query_id_count(&temp, &root, &[old.id.to_hex()]), 1);
    assert_eq!(
        query_id_count(
            &temp,
            completed["root"].as_str().unwrap(),
            &[old.id.to_hex(), first.id.to_hex(), second.id.to_hex(),]
        ),
        3,
        "new root must retain history and both tied posts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exact_count_detects_hidden_tied_event_and_preserves_exact_checkpoint() {
    let temp = TempDir::new().unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let old = event(&alice, 1, "archive");
    let ready = event(&alice, 20, "durable prior author");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n{}\n", alice.public_key(), bob.public_key()),
    )
    .unwrap();
    let relay = Relay::new(vec![
        ready.clone(),
        event(&bob, 30, "tie one"),
        event(&bob, 30, "tie two"),
        event(&bob, 30, "hidden third event"),
    ])
    .await;
    relay.event_page_cap.store(2, Ordering::Relaxed);
    let first = success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100", "--max-authors-per-run", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(first["next_author"], 1);
    assert!(relay.count_filters.lock().unwrap().is_empty());
    let state_file = temp.path().join("data/nostr-index/catchup-state.json");
    let saved = std::fs::read(&state_file).unwrap();
    let failed = catchup(&temp, &root, &relay).output().unwrap();
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success());
    assert!(
        error.contains("ambiguous capped timestamp 30") && error.contains("coverage incomplete"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&state_file).unwrap(),
        saved,
        "incomplete author cannot rewrite the prior durable checkpoint"
    );
    assert_eq!(checkpoint(&temp)["root"], first["root"]);
    assert_eq!(checkpoint(&temp)["next_author"], 1);
    assert_eq!(
        *relay.count_filters.lock().unwrap(),
        vec![
            json!({"authors": [bob.public_key().to_hex()], "kinds": [1, 5], "since": 30, "until": 30})
        ],
    );
    assert_eq!(query_id_count(&temp, &root, &[old.id.to_hex()]), 1);
    assert_eq!(
        query_id_count(
            &temp,
            first["root"].as_str().unwrap(),
            &[old.id.to_hex(), ready.id.to_hex()]
        ),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_capacity_probe_is_cached_and_excludes_witness_events_from_author_index() {
    let temp = TempDir::new().unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let witness = Keys::generate();
    let old = event(&alice, 1, "retained archive");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!(
            "{}\n{}\n{}\n{}\n",
            alice.public_key(),
            bob.public_key(),
            carol.public_key(),
            dave.public_key()
        ),
    )
    .unwrap();
    let incoming = vec![
        event(&alice, 20, "alice one"),
        event(&alice, 20, "alice two"),
        event(&bob, 30, "bob one"),
        event(&bob, 30, "bob two"),
        event(&carol, 40, "carol one"),
        event(&carol, 40, "carol two"),
        event(&dave, 50, "dave one"),
        event(&dave, 50, "dave two"),
    ];
    // These are the first three results of the author-free page, but their
    // author is not eligible for this run. They prove capacity, not coverage.
    let witnesses = vec![
        event(&witness, 80, "witness one"),
        event(&witness, 81, "witness two"),
        event(&witness, 82, "witness three"),
    ];
    let relay = Relay::new(incoming.iter().chain(&witnesses).cloned().collect()).await;
    relay.count_silent.store(true, Ordering::Relaxed);
    let completed = success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100"])
            .output()
            .unwrap(),
    );
    assert_eq!(completed["next_author"], 4);
    assert_eq!(completed["complete"], true);
    assert_eq!(
        completed["events_received"], 8,
        "probe events must not count as author results"
    );
    assert_eq!(completed["pass_until"], 100);
    assert_eq!(
        *relay.capacity_filters.lock().unwrap(),
        vec![json!({"kinds": [1, 5], "since": 10, "until": 100, "limit": 4}); 2],
        "each of two source workers reuses its exact pass/kinds/limit probe across two authors",
    );
    assert!(
        relay.count_filters.lock().unwrap().is_empty(),
        "observed capacity above the unsaturated tie must avoid unavailable COUNT"
    );
    assert_eq!(relay.connections.load(Ordering::Relaxed), 2);
    let next_root = completed["root"].as_str().unwrap();
    assert_eq!(query_id_count(&temp, &root, &[old.id.to_hex()]), 1);
    let wanted = std::iter::once(old.id.to_hex())
        .chain(incoming.iter().map(|event| event.id.to_hex()))
        .collect::<Vec<_>>();
    assert_eq!(query_id_count(&temp, next_root, &wanted), 9);
    let excluded = witnesses
        .iter()
        .map(|event| event.id.to_hex())
        .collect::<Vec<_>>();
    assert_eq!(
        query_id_count(&temp, next_root, &excluded),
        0,
        "capacity-only witness events must never enter the author index"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_uniform_cap_with_unavailable_count_preserves_exact_checkpoint() {
    let temp = TempDir::new().unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let witness = Keys::generate();
    let old = event(&alice, 1, "archive");
    let ready = event(&alice, 20, "durable prior author");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n{}\n", alice.public_key(), bob.public_key()),
    )
    .unwrap();
    let relay = Relay::new(vec![
        ready.clone(),
        event(&bob, 30, "tie one"),
        event(&bob, 30, "tie two"),
        event(&bob, 30, "hidden third event"),
        event(&witness, 80, "witness one"),
        event(&witness, 81, "witness two"),
        event(&witness, 82, "witness three"),
    ])
    .await;
    relay.event_page_cap.store(2, Ordering::Relaxed);
    relay.count_supported.store(false, Ordering::Relaxed);
    let first = success(
        catchup(&temp, &root, &relay)
            .args(["--until", "100", "--max-authors-per-run", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(first["next_author"], 1);
    let state_file = temp.path().join("data/nostr-index/catchup-state.json");
    let saved = std::fs::read(&state_file).unwrap();
    let failed = catchup(&temp, &root, &relay).output().unwrap();
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success());
    assert!(
        error.contains("ambiguous capped timestamp 30") && error.contains("coverage incomplete"),
        "{error}"
    );
    assert_eq!(std::fs::read(&state_file).unwrap(), saved);
    assert_eq!(checkpoint(&temp)["root"], first["root"]);
    assert_eq!(checkpoint(&temp)["next_author"], 1);
    assert_eq!(
        *relay.capacity_filters.lock().unwrap(),
        vec![json!({"kinds": [1, 5], "since": 10, "until": 100, "limit": 4})]
    );
    assert_eq!(
        *relay.count_filters.lock().unwrap(),
        vec![
            json!({"authors": [bob.public_key().to_hex()], "kinds": [1, 5], "since": 30, "until": 30})
        ]
    );
    assert_eq!(query_id_count(&temp, &root, &[old.id.to_hex()]), 1);
    assert_eq!(
        query_id_count(
            &temp,
            first["root"].as_str().unwrap(),
            &[old.id.to_hex(), ready.id.to_hex()]
        ),
        2
    );
}

/// Run only inside the explicitly provisioned private 128 MiB tmpfs. This is
/// production CLI, real signed events, real LMDB/catalog and external writes.
#[cfg(all(target_os = "linux", feature = "lmdb"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires HTREE_CAPACITY_TEST_ROOT on a private 128 MiB tmpfs"]
async fn cli_physical_capacity_stops_tag_replacement_and_resumes_exact_checkpoint() {
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    const MIB: u64 = 1024 * 1024;
    const FLOOR: u64 = 32 * MIB;
    fn available(path: &std::path::Path) -> u64 {
        let file = File::open(path).unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        assert_eq!(
            unsafe { libc::fstatvfs(file.as_raw_fd(), stat.as_mut_ptr()) },
            0
        );
        let stat = unsafe { stat.assume_init() };
        stat.f_bavail * stat.f_frsize
    }
    let fixture = std::path::PathBuf::from(
        std::env::var_os("HTREE_CAPACITY_TEST_ROOT").expect("explicit isolated tmpfs required"),
    );
    let fd = File::open(&fixture).unwrap();
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    assert_eq!(unsafe { libc::fstatfs(fd.as_raw_fd(), fs.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { fs.assume_init() }.f_type, libc::TMPFS_MAGIC);
    assert!(available(&fixture) > 100 * MIB && available(&fixture) <= 128 * MIB);
    let temp = TempDir::new_in(&fixture).unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let old = event(&alice, 1, "retained history");
    let tags = (0..5000)
        .map(|i| nostr::Tag::parse(["p".to_owned(), format!("{i:064x}")]).unwrap())
        .collect::<Vec<_>>();
    let old_contacts = EventBuilder::new(Kind::ContactList, "old contacts")
        .tags(tags.clone())
        .custom_created_at(Timestamp::from_secs(2))
        .sign_with_keys(&bob)
        .unwrap();
    let replacement = EventBuilder::new(Kind::ContactList, "new contacts")
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&bob)
        .unwrap();
    let root = import_events_with_external(&temp, &[old.clone(), old_contacts.clone()], true);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!(
            "{}\n{}\n",
            alice.public_key().to_hex(),
            bob.public_key().to_hex()
        ),
    )
    .unwrap();
    let relay = Relay::new(vec![
        event(&alice, 20, "durable first author"),
        replacement.clone(),
    ])
    .await;
    let guarded = || {
        let mut cmd = catchup_with_floor(&temp, &root, &relay, FLOOR);
        cmd.args(["--kind", "3"]);
        cmd
    };
    let first = success(
        guarded()
            .args(["--until", "100", "--max-authors-per-run", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(first["next_author"], 1);
    let state_path = temp.path().join("data/nostr-index/catchup-state.json");
    let durable_bytes = std::fs::read(&state_path).unwrap();
    let pressure_path = temp.path().join("fixture-pressure");
    let mut pressure = File::create(&pressure_path).unwrap();
    let leave = FLOOR + hashtree_lmdb::PHYSICAL_SPACE_METADATA_MARGIN + MIB;
    let mut fill = available(&fixture).checked_sub(leave).unwrap();
    let chunk = vec![0x5a; MIB as usize];
    while fill > 0 {
        let n = fill.min(MIB) as usize;
        pressure.write_all(&chunk[..n]).unwrap();
        fill -= n as u64;
    }
    pressure.sync_all().unwrap();
    drop(pressure);
    let failed = guarded().output().unwrap();
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(
        !failed.status.success(),
        "tag projection must exceed the remaining write allowance"
    );
    assert!(
        error.contains("append catchup author 1"),
        "failure must reach actual indexing: {error}"
    );
    assert!(
        error.contains("physical-space admission") && error.contains("latched_errno=28"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&state_path).unwrap(),
        durable_bytes,
        "failed author cannot advance durable state"
    );
    assert!(
        available(&fixture) >= FLOOR,
        "guard must preserve the physical floor"
    );
    std::fs::remove_file(&pressure_path).unwrap();
    let resumed = success(guarded().output().unwrap());
    assert_eq!(resumed["next_author"], 2);
    assert_eq!(resumed["pass_until"], 100);
    let find = |root: &str, id: String| {
        success(
            command(&temp)
                .args([
                    "nostr-index",
                    "query",
                    "--root",
                    root,
                    "--filter",
                    &json!({"ids":[id]}).to_string(),
                    "--limit",
                    "10",
                ])
                .output()
                .unwrap(),
        )["count"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(find(&root, old.id.to_hex()), 1);
    assert_eq!(
        find(&root, old_contacts.id.to_hex()),
        1,
        "original replaceable history remains reachable by old root"
    );
    assert_eq!(find(resumed["root"].as_str().unwrap(), old.id.to_hex()), 1);
    assert_eq!(
        find(resumed["root"].as_str().unwrap(), replacement.id.to_hex()),
        1
    );
    assert_eq!(
        find(resumed["root"].as_str().unwrap(), old_contacts.id.to_hex()),
        0
    );
    println!("capacity acceptance: floor={FLOOR}, free_after_resume={}, previous_checkpoint_preserved=true", available(&fixture));
}
