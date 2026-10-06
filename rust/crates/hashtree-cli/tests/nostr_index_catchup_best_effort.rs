// Real CLI fixtures: unavailable sources cannot stop complete sources or falsify coverage.
use futures::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind, Timestamp};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_tungstenite::{accept_async, tungstenite::Message};

struct Relay {
    url: String,
    failing: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    authors_seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(events: Vec<Event>, fail: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let failing = Arc::new(AtomicBool::new(fail));
        let requests = Arc::new(AtomicUsize::new(0));
        let authors_seen = Arc::new(Mutex::new(Vec::new()));
        let (failing_task, requests_task, authors_task) =
            (failing.clone(), requests.clone(), authors_seen.clone());
        let events = Arc::new(events);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let (events, failing, requests, authors) = (events.clone(), failing_task.clone(), requests_task.clone(), authors_task.clone());
                        connections.spawn(async move {
                            let Ok(mut socket) = accept_async(stream).await else { return };
                            while let Some(Ok(Message::Text(text))) = socket.next().await {
                                let message: Value = serde_json::from_str(&text).unwrap();
                                if message[0] == "CLOSE" { continue; }
                                assert_eq!(message[0], "REQ");
                                requests.fetch_add(1, Ordering::SeqCst);
                                let (subscription, filter) = (&message[1], &message[2]);
                                let author = filter["authors"][0].as_str().unwrap();
                                authors.lock().unwrap().push(author.to_owned());
                                let mut matches = events.iter().filter(|event| {
                                    event.pubkey.to_hex() == author
                                        && event.created_at.as_secs() >= filter["since"].as_u64().unwrap()
                                        && event.created_at.as_secs() <= filter["until"].as_u64().unwrap()
                                }).cloned().collect::<Vec<_>>();
                                matches.sort_by_key(|event| std::cmp::Reverse(event.created_at));
                                matches.truncate(filter["limit"].as_u64().unwrap() as usize);
                                for event in matches {
                                    if socket.send(Message::Text(json!(["EVENT",subscription,event]).to_string())).await.is_err() { return; }
                                }
                                let terminal = if failing.load(Ordering::SeqCst) {
                                    json!(["CLOSED",subscription,"blocked: unavailable fixture"])
                                } else { json!(["EOSE",subscription]) };
                                if socket.send(Message::Text(terminal.to_string())).await.is_err() { return; }
                            }
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => { result.unwrap(); }
                }
            }
        });
        Self {
            url,
            failing,
            requests,
            authors_seen,
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
        "[storage]\nmax_size_gb = 1\nevict_orphans = false\n[server]\nenable_fips_udp = false\nenable_fips_webrtc = false\nenable_fips_lan_discovery = false\nfips_relays = []\nfips_request_timeout_ms = 250\nfips_discovery_scope = \"catchup-isolated-test\"\n",
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

fn catchup(temp: &TempDir, root: &str, relays: &[&Relay]) -> Command {
    let mut cmd = command(temp);
    cmd.args(["nostr-index", "catch-up", "--root", root, "--authors-file"])
        .arg(temp.path().join("authors.txt"))
        .args([
            "--source-mode",
            "best-effort",
            "--min-free-bytes",
            "0",
            "--since",
            "10",
            "--until",
            "100",
            "--kind",
            "1",
            "--kind",
            "5",
            "--page-size",
            "4",
            "--fetch-timeout-secs",
            "2",
        ]);
    for relay in relays {
        cmd.args(["--relay", &relay.url]);
    }
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

fn fixture() -> (Vec<String>, Vec<Event>, Vec<Event>, Vec<Event>) {
    let keys = (1..=5)
        .map(|key| Keys::parse(&format!("{key:064x}")).unwrap())
        .collect::<Vec<_>>();
    let authors = keys.iter().map(|key| key.public_key().to_hex()).collect();
    let old = keys
        .iter()
        .enumerate()
        .map(|(i, key)| event(key, i as u64 + 1, "retained history"))
        .collect();
    let healthy = keys
        .iter()
        .enumerate()
        .map(|(i, key)| event(key, i as u64 + 20, "complete source note"))
        .collect();
    let partial = keys
        .iter()
        .enumerate()
        .map(|(i, key)| event(key, i as u64 + 40, "incomplete source must be discarded"))
        .collect();
    (authors, old, healthy, partial)
}

fn coverage_chain(temp: &TempDir) -> Vec<Value> {
    let mut head = checkpoint(temp)["coverage_head"]
        .as_str()
        .map(str::to_owned);
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some(hash) = head {
        assert!(seen.insert(hash.clone()), "coverage chain must not cycle");
        assert_eq!(hash.len(), 64);
        let bytes = std::fs::read(
            temp.path()
                .join("data/nostr-index/catchup-coverage")
                .join(format!("{hash}.json")),
        )
        .unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(&bytes)),
            hash,
            "exact content-addressed durable receipt"
        );
        let receipt: Value = serde_json::from_slice(&bytes).unwrap();
        head = receipt["previous_head"].as_str().map(str::to_owned);
        records.push(receipt);
        assert!(records.len() <= 5);
    }
    records.reverse();
    records
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_unavailable_source_is_quarantined_while_complete_source_advances_all_authors() {
    let (authors, old, healthy, partial) = fixture();
    let temp = TempDir::new().unwrap();
    let root = seed(&temp, &old, &authors).await;
    let good = Relay::new(healthy.clone(), false).await;
    let bad = Relay::new(partial, true).await;
    let result = success(output(catchup(&temp, &root, &[&good, &bad])).await);
    assert_eq!(result["next_author"], 5);
    assert_eq!(result["events_received"], 5);
    assert_eq!(checkpoint(&temp)["policy"]["source_mode"], "best-effort");
    let attempts = bad.requests.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&attempts),
        "failed source gets at most two already-in-flight attempts, got {attempts}"
    );
    let found = query(&temp, result["root"].as_str().unwrap()).await;
    let actual = found["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    let expected = old
        .iter()
        .chain(&healthy)
        .map(|e| e.id.to_hex())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, expected,
        "partial failed-source events cannot escape into index"
    );
    assert_eq!(
        query(&temp, &root).await["count"],
        5,
        "original root remains readable"
    );
    let records = coverage_chain(&temp);
    assert_eq!(records.len(), 5);
    for (ordinal, receipt) in records.iter().enumerate() {
        assert_eq!(receipt["ordinal"], ordinal);
        assert_eq!(receipt["author"], authors[ordinal]);
        assert_eq!(receipt["pass_since"], 10);
        assert_eq!(receipt["pass_until"], 100);
        let outcomes = receipt["sources"].as_array().unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes
            .iter()
            .any(|s| s["relay"] == good.url && s["status"] == "complete"));
        assert!(outcomes.iter().any(
            |s| s["relay"] == bad.url && (s["status"] == "failed" || s["status"] == "skipped")
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_all_sources_unavailable_preserves_frontier_and_recovers_on_next_invocation() {
    let (authors, old, healthy, _) = fixture();
    let temp = TempDir::new().unwrap();
    let root = seed(&temp, &old, &authors).await;
    let relay = Relay::new(healthy, true).await;
    let failed = output(catchup(&temp, &root, &[&relay])).await;
    assert!(!failed.status.success());
    let saved = checkpoint(&temp);
    assert_eq!(saved["next_author"], 0);
    assert_eq!(saved["events_received"], 0);
    assert_eq!(saved["root"], root);
    assert!(saved.get("coverage_head").is_none());
    assert!(coverage_chain(&temp).is_empty());
    relay.failing.store(false, Ordering::SeqCst);
    let result = success(output(catchup(&temp, &root, &[&relay])).await);
    assert_eq!(result["next_author"], 5);
    assert_eq!(result["events_received"], 5);
    assert_eq!(coverage_chain(&temp).len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_resume_requires_durable_coverage_and_never_refetches_committed_prefix() {
    let (authors, old, healthy, _) = fixture();
    let temp = TempDir::new().unwrap();
    let root = seed(&temp, &old, &authors).await;
    let relay = Relay::new(healthy, false).await;
    let mut first = catchup(&temp, &root, &[&relay]);
    first.args(["--max-authors-per-run", "2"]);
    success(output(first).await);
    let saved = checkpoint(&temp);
    assert_eq!(saved["next_author"], 2);
    assert_eq!(coverage_chain(&temp).len(), 2);
    let head = saved["coverage_head"].as_str().unwrap();
    let receipt_file = temp
        .path()
        .join("data/nostr-index/catchup-coverage")
        .join(format!("{head}.json"));
    let original = std::fs::read(&receipt_file).unwrap();
    std::fs::write(&receipt_file, b"{}\n").unwrap();
    assert!(!output(catchup(&temp, &root, &[&relay]))
        .await
        .status
        .success());
    assert_eq!(
        checkpoint(&temp),
        saved,
        "bad durable head cannot commit a new frontier"
    );
    std::fs::write(&receipt_file, original).unwrap();
    relay.authors_seen.lock().unwrap().clear();
    success(output(catchup(&temp, &root, &[&relay])).await);
    assert_eq!(checkpoint(&temp)["next_author"], 5);
    assert_eq!(coverage_chain(&temp).len(), 5);
    assert!(relay
        .authors_seen
        .lock()
        .unwrap()
        .iter()
        .all(|a| !authors[..2].contains(a)));
}
