use std::collections::BTreeSet;
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
use tokio::sync::{Barrier, Notify};
use tokio::task::JoinSet;
use tokio_tungstenite::{accept_async, tungstenite::Message};

struct RelayState {
    events: Vec<Event>,
    authors: Vec<String>,
    barrier_enabled: AtomicBool,
    barrier_seen: Mutex<BTreeSet<String>>,
    barrier: Barrier,
    fail_second: AtomicBool,
    failed_socket_dropped: Notify,
    filters: Mutex<Vec<Value>>,
    connections: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

struct ActiveQuery(Arc<RelayState>);

impl ActiveQuery {
    fn new(state: &Arc<RelayState>) -> Self {
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_active.fetch_max(active, Ordering::SeqCst);
        Self(state.clone())
    }
}

impl Drop for ActiveQuery {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Relay {
    url: String,
    state: Arc<RelayState>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(events: Vec<Event>, authors: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let state = Arc::new(RelayState {
            events,
            authors,
            barrier_enabled: AtomicBool::new(true),
            barrier_seen: Mutex::new(BTreeSet::new()),
            barrier: Barrier::new(2),
            fail_second: AtomicBool::new(false),
            failed_socket_dropped: Notify::new(),
            filters: Mutex::new(Vec::new()),
            connections: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            // Dropping this set aborts connection handlers as well as accept.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        task_state.connections.fetch_add(1, Ordering::SeqCst);
                        let state = task_state.clone();
                        connections.spawn(async move {
                            let Ok(mut socket) = accept_async(stream).await else { return };
                            while let Some(Ok(Message::Text(text))) = socket.next().await {
                                let message: Value = serde_json::from_str(&text).unwrap();
                                if message[0] == "CLOSE" {
                                    continue;
                                }
                                assert_eq!(message[0], "REQ", "distinct timestamps need no probes");
                                let subscription = &message[1];
                                let filter = &message[2];
                                let author = filter["authors"][0].as_str().unwrap();
                                state.filters.lock().unwrap().push(filter.clone());
                                let _active = ActiveQuery::new(&state);
                                let at_barrier = state.barrier_enabled.load(Ordering::SeqCst)
                                    && state.authors[..2].iter().any(|key| key == author)
                                    && state.barrier_seen.lock().unwrap().insert(author.to_owned());
                                if at_barrier {
                                    // A serial fetcher cannot receive either initial EOSE.
                                    state.barrier.wait().await;
                                }
                                let fail = state.fail_second.load(Ordering::SeqCst);
                                let mut matched = state.events.iter().filter(|event| {
                                    event.pubkey.to_hex() == author
                                        && filter["kinds"].as_array().unwrap().iter().any(|kind| {
                                            kind.as_u64() == Some(u64::from(event.kind.as_u16()))
                                        })
                                        && event.created_at.as_secs() >= filter["since"].as_u64().unwrap()
                                        && event.created_at.as_secs() <= filter["until"].as_u64().unwrap()
                                }).cloned().collect::<Vec<_>>();
                                matched.sort_by_key(|event| std::cmp::Reverse(event.created_at));
                                matched.truncate(filter["limit"].as_u64().unwrap() as usize);
                                if fail && author == state.authors[1] {
                                    // Verified partial data must not escape a terminal page failure.
                                    if let Some(event) = matched.first() {
                                        let _ = socket.send(Message::Text(
                                            json!(["EVENT", subscription, event]).to_string()
                                        )).await;
                                    }
                                    let _ = socket.send(Message::Text(
                                        json!(["CLOSED", subscription, "blocked: fixture failure"]).to_string()
                                    )).await;
                                    // Observe the client's invalidation before allowing author 0
                                    // to finish. This makes the later failure known before the
                                    // prior checkpoint can release a slot for author 2.
                                    while let Some(Ok(_)) = socket.next().await {}
                                    state.failed_socket_dropped.notify_one();
                                    return;
                                }
                                if fail && at_barrier && author == state.authors[0] {
                                    state.failed_socket_dropped.notified().await;
                                }
                                for event in matched {
                                    if socket.send(Message::Text(
                                        json!(["EVENT", subscription, event]).to_string()
                                    )).await.is_err() { return }
                                }
                                if socket.send(Message::Text(
                                    json!(["EOSE", subscription]).to_string()
                                )).await.is_err() { return }
                            }
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    }
                }
            }
        });
        Self { url, state, task }
    }

    fn filters(&self) -> Vec<Value> {
        self.state.filters.lock().unwrap().clone()
    }

    fn disable_barrier(&self) {
        self.state.barrier_enabled.store(false, Ordering::SeqCst);
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
        .arg("--data-dir")
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

async fn output(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

async fn seed(temp: &TempDir, events: &[Event], authors: &[String]) -> String {
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/config.toml"),
        "[storage]\nmax_size_gb = 1\nevict_orphans = false\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n", authors.join("\n")),
    )
    .unwrap();
    let events_file = temp.path().join("events.json");
    std::fs::write(&events_file, serde_json::to_vec(events).unwrap()).unwrap();
    let mut cmd = command(temp);
    cmd.args(["nostr-index", "import", "--events"])
        .arg(events_file);
    success(output(cmd).await)["root"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn catchup(temp: &TempDir, root: &str, relay: &Relay) -> Command {
    let mut cmd = command(temp);
    cmd.args(["nostr-index", "catch-up", "--root", root, "--authors-file"])
        .arg(temp.path().join("authors.txt"))
        .args([
            "--min-free-bytes",
            "0",
            "--since",
            "10",
            "--until",
            "100",
            "--relay",
            &relay.url,
            "--kind",
            "1",
            "--kind",
            "5",
            "--page-size",
            "4",
            "--fetch-timeout-secs",
            "2",
        ]);
    cmd
}

fn checkpoint_bytes(temp: &TempDir) -> Vec<u8> {
    std::fs::read(temp.path().join("data/nostr-index/catchup-state.json")).unwrap()
}

fn checkpoint(temp: &TempDir) -> Value {
    serde_json::from_slice(&checkpoint_bytes(temp)).unwrap()
}

async fn query(temp: &TempDir, root: &str) -> Value {
    let mut cmd = command(temp);
    cmd.args([
        "nostr-index",
        "query",
        "--root",
        root,
        "--filter",
        "{}",
        "--limit",
        "100",
    ]);
    success(output(cmd).await)
}

fn sorted_filters(mut filters: Vec<Value>) -> Vec<Value> {
    filters.sort_by_key(Value::to_string);
    filters
}

fn fixture() -> (Vec<String>, Vec<Event>, Vec<Event>) {
    let keys = (1..=3)
        .map(|key| Keys::parse(&format!("{key:064x}")).unwrap())
        .collect::<Vec<_>>();
    let authors = keys.iter().map(|keys| keys.public_key().to_hex()).collect();
    let historical = keys
        .iter()
        .enumerate()
        .map(|(i, keys)| event(keys, i as u64 + 1, "retained before the catchup floor"))
        .collect();
    let incoming = keys
        .iter()
        .enumerate()
        .flat_map(|(i, keys)| {
            [
                event(keys, 20 + i as u64 * 10, "older new note"),
                event(keys, 21 + i as u64 * 10, "newer new note"),
            ]
        })
        .collect();
    (authors, historical, incoming)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_fetches_two_authors_concurrently_with_sequential_root_and_history() {
    let (authors, historical, incoming) = fixture();
    let parallel = TempDir::new().unwrap();
    let sequential = TempDir::new().unwrap();
    let root = seed(&parallel, &historical, &authors).await;
    assert_eq!(seed(&sequential, &historical, &authors).await, root);
    let relay = Relay::new(incoming.clone(), authors).await;

    let completed = success(output(catchup(&parallel, &root, &relay)).await);
    assert_eq!(completed["complete"], true);
    assert_eq!(completed["next_author"], 3);
    assert_eq!(completed["events_received"], 6);
    assert_eq!(relay.state.barrier_seen.lock().unwrap().len(), 2);
    assert_eq!(relay.state.max_active.load(Ordering::SeqCst), 2);
    assert_eq!(relay.state.connections.load(Ordering::SeqCst), 2);
    let parallel_filters = relay.filters();
    assert_eq!(
        parallel_filters.len(),
        9,
        "one broad, boundary, and empty page per author"
    );

    relay.disable_barrier();
    relay.state.filters.lock().unwrap().clear();
    let mut serial_result = Value::Null;
    for ordinal in 1..=3 {
        let mut cmd = catchup(&sequential, &root, &relay);
        cmd.args(["--max-authors-per-run", "1"]);
        serial_result = success(output(cmd).await);
        assert_eq!(serial_result["next_author"], ordinal);
    }
    assert_eq!(
        serial_result, completed,
        "same frozen signed inputs must produce the exact root"
    );
    assert_eq!(checkpoint_bytes(&parallel), checkpoint_bytes(&sequential));
    assert_eq!(
        sorted_filters(parallel_filters),
        sorted_filters(relay.filters())
    );
    let final_root = completed["root"].as_str().unwrap();
    let current = query(&parallel, final_root).await;
    assert_eq!(current, query(&sequential, final_root).await);
    let expected = historical
        .iter()
        .chain(&incoming)
        .map(|event| event.id.to_hex())
        .collect::<BTreeSet<_>>();
    let actual = current["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    assert_eq!(
        query(&parallel, &root).await,
        query(&sequential, &root).await
    );
    assert_eq!(query(&parallel, &root).await["count"], historical.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_later_author_failure_keeps_prior_checkpoint_and_resumes_exactly() {
    let (authors, historical, incoming) = fixture();
    let parallel = TempDir::new().unwrap();
    let sequential = TempDir::new().unwrap();
    let root = seed(&parallel, &historical, &authors).await;
    assert_eq!(seed(&sequential, &historical, &authors).await, root);
    let relay = Relay::new(incoming, authors.clone()).await;
    relay.state.fail_second.store(true, Ordering::SeqCst);

    let failed = output(catchup(&parallel, &root, &relay)).await;
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("relay CLOSED subscription before EOSE")
    );
    assert_eq!(checkpoint(&parallel)["next_author"], 1);
    assert_eq!(checkpoint(&parallel)["events_received"], 2);
    assert_eq!(relay.state.max_active.load(Ordering::SeqCst), 2);
    assert_eq!(relay.state.connections.load(Ordering::SeqCst), 2);
    assert!(
        relay
            .filters()
            .iter()
            .all(|filter| filter["authors"][0] != authors[2]),
        "known failure must stop author 2 admission"
    );
    assert_eq!(
        relay.filters().len(),
        4,
        "prior author completes three pages, failed author only one"
    );
    let durable = checkpoint_bytes(&parallel);

    relay.disable_barrier();
    relay.state.fail_second.store(false, Ordering::SeqCst);
    let mut first = catchup(&sequential, &root, &relay);
    first.args(["--max-authors-per-run", "1"]);
    let first = success(output(first).await);
    assert_eq!(
        durable,
        checkpoint_bytes(&sequential),
        "no partial event or out-of-order root may enter the checkpoint"
    );
    assert_eq!(
        query(&parallel, first["root"].as_str().unwrap()).await,
        query(&sequential, first["root"].as_str().unwrap()).await
    );
    for _ in 1..3 {
        let mut cmd = catchup(&sequential, &root, &relay);
        cmd.args(["--max-authors-per-run", "1"]);
        success(output(cmd).await);
    }
    relay.state.filters.lock().unwrap().clear();
    let resumed = success(output(catchup(&parallel, &root, &relay)).await);
    assert_eq!(resumed["complete"], true);
    assert_eq!(resumed["pass_since"], 10);
    assert_eq!(resumed["pass_until"], 100);
    assert_eq!(checkpoint_bytes(&parallel), checkpoint_bytes(&sequential));
    assert_eq!(relay.filters().len(), 6);
    assert!(
        relay
            .filters()
            .iter()
            .all(|filter| filter["authors"][0] != authors[0]),
        "resume must not refetch the committed author"
    );
    let final_root = resumed["root"].as_str().unwrap();
    assert_eq!(
        query(&parallel, final_root).await,
        query(&sequential, final_root).await
    );
    assert_eq!(query(&parallel, &root).await["count"], historical.len());
}
