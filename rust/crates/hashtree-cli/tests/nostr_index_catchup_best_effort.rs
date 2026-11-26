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
        (1..=4).contains(&attempts),
        "failed source gets at most four already-in-flight attempts, got {attempts}"
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

#[cfg(feature = "lmdb")]
async fn damaged_note_fixture(target_index: usize) -> (TempDir, String, Vec<String>, Event) {
    damaged_note_fixture_with_order(target_index, false).await
}

#[cfg(feature = "lmdb")]
async fn damaged_note_fixture_with_order(
    target_index: usize,
    wide: bool,
) -> (TempDir, String, Vec<String>, Event) {
    use hashtree_cli::HashtreeStore;
    use hashtree_config::StorageBackend;
    use hashtree_core::{nhash_decode, nhash_encode_full, Cid, NHashData};
    use hashtree_index::{BTree, BTreeOptions};
    use hashtree_nostr::{
        NostrEventIndex, NostrEventStore, NostrEventStoreOptions, StoredNostrEvent,
    };
    let temp = TempDir::new().unwrap();
    let (authors, mut old, _, _) = fixture();
    if wide {
        let keys = Keys::parse(&format!("{:064x}", target_index + 1)).unwrap();
        old[target_index] = EventBuilder::new(Kind::TextNote, "retained tagged target")
            .custom_created_at(Timestamp::from_secs(target_index as u64 + 1))
            .tags([nostr::Tag::parse(["t", "wide-fixture"]).unwrap()])
            .sign_with_keys(&keys)
            .unwrap();
        for ordinal in 0..180 {
            old.push(
                EventBuilder::new(Kind::TextNote, format!("retained tagged note {ordinal}"))
                    .custom_created_at(Timestamp::from_secs(100 + ordinal))
                    .tags([nostr::Tag::parse(["t", "wide-fixture"]).unwrap()])
                    .sign_with_keys(&keys)
                    .unwrap(),
            );
        }
    }
    let target = old[if target_index == usize::MAX {
        0
    } else {
        target_index
    }]
    .clone();
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    for kind in [0u16, 3, 30023] {
        old.push(
            EventBuilder::new(Kind::from(kind), "retained metadata")
                .custom_created_at(Timestamp::from_secs(2))
                .tags([nostr::Tag::parse(["d", "fixture"]).unwrap()])
                .sign_with_keys(&keys)
                .unwrap(),
        );
    }
    if wide {
        assert_eq!(old.len(), 188);
        assert_eq!(
            old.iter()
                .filter(|event| event.kind == Kind::TextNote && !event.tags.is_empty())
                .count(),
            181
        );
    }
    let original = seed(&temp, &old, &authors).await;
    let decoded = nhash_decode(&original).unwrap();
    let original = Cid {
        hash: decoded.hash,
        key: decoded.decrypt_key,
    };
    let backing = HashtreeStore::with_options_and_backend(
        temp.path().join("data"),
        None,
        1024 * 1024 * 1024,
        false,
        &StorageBackend::Lmdb,
    )
    .unwrap();
    let events = NostrEventStore::with_options(
        backing.store_arc(),
        NostrEventStoreOptions {
            btree_order: wide.then_some(64),
            ..Default::default()
        },
    );
    let original = if wide {
        let signed = old.iter().map(|event| {
            serde_json::from_value::<StoredNostrEvent>(serde_json::to_value(event).unwrap())
                .unwrap()
        });
        events.build(None, signed).await.unwrap().unwrap()
    } else {
        original
    };
    let tree =
        hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(backing.store_arc()));
    let entries = tree.list_directory_required(&original).await.unwrap();
    let mut roots = NostrEventIndex::ALL
        .into_iter()
        .map(|name| {
            let cid = entries
                .iter()
                .find(|entry| entry.name == name.name())
                .map(|entry| Cid {
                    hash: entry.hash,
                    key: entry.key,
                });
            (name, cid)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let index = BTree::new(backing.store_arc(), BTreeOptions::default());
    let by_id = index
        .update_links(
            roots[&NostrEventIndex::ById].as_ref(),
            [(target.id.to_hex(), None)],
        )
        .await
        .unwrap();
    roots.insert(NostrEventIndex::ById, by_id);
    if target_index == usize::MAX {
        let empty = tree.put_directory(Vec::new()).await.unwrap();
        roots = NostrEventIndex::ALL
            .into_iter()
            .map(|name| (name, Some(empty.clone())))
            .collect();
    }
    let root = events
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    events.get_canonical_manifest(&root).await.unwrap();
    backing.force_sync().unwrap();
    let root = nhash_encode_full(&NHashData {
        hash: root.hash,
        decrypt_key: root.key,
    })
    .unwrap();
    (temp, root, authors, target)
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn catchup_reconciles_pre_window_ids_with_zero_fetched_event_delta() {
    let (temp, root, _, target) = damaged_note_fixture(0).await;
    let relay = Relay::new(Vec::new(), false).await;
    let mut cmd = catchup(&temp, &root, &[&relay]);
    cmd.args(["--max-authors-per-run", "1"]);
    let result = success(output(cmd).await);
    assert_eq!(result["events_received"], 0);
    assert_eq!(result["next_author"], 1);
    assert_ne!(result["root"], root);
    let saved = checkpoint(&temp);
    let head = saved["coverage_head"].as_str().unwrap();
    let proof: Value = serde_json::from_slice(
        &std::fs::read(
            temp.path()
                .join(format!("data/nostr-index/catchup-coverage/{head}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(proof["before_root"], root);
    assert_eq!(proof["after_root"], result["root"]);
    assert_eq!(proof["events_received"], 0);
    let mut query = command(&temp);
    query.args([
        "nostr-index",
        "query",
        "--root",
        result["root"].as_str().unwrap(),
        "--filter",
        &json!({"ids":[target.id.to_hex()]}).to_string(),
        "--limit",
        "1",
    ]);
    let found = success(output(query).await);
    assert_eq!(found["count"], 1);
    assert_eq!(found["events"][0]["id"], target.id.to_hex());
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn native_repair_witness_resumes_exact_coverage_once_then_becomes_inert() {
    // Realistic producer mismatch: 188 signed events (181 tagged notes) built at order64, then
    // the actual previous repair executable upserts at its default order32.
    let (temp, root, _, target) = damaged_note_fixture_with_order(1, true).await;
    let relay = Relay::new(Vec::new(), false).await;
    let sha = |bytes: &[u8]| hex::encode(Sha256::digest(bytes));
    let mut first = catchup(&temp, &root, &[&relay]);
    first.args(["--max-authors-per-run", "1"]);
    success(output(first).await);
    let before_bytes = checkpoint_bytes(&temp);
    let before: Value = serde_json::from_slice(&before_bytes).unwrap();
    let before_file = temp.path().join("before-checkpoint.json");
    std::fs::write(&before_file, &before_bytes).unwrap();
    let event_file = temp.path().join("repair-event.json");
    let event_bytes = serde_json::to_vec(&target).unwrap();
    std::fs::write(&event_file, &event_bytes).unwrap();
    let native_file = temp.path().join("native-repair.json");
    // Fleet sets this to the exact previous ff63 artifact: the fixture then
    // exercises the already-deployed repair command and the successor resume.
    let repair_bin = std::fs::canonicalize(
        std::env::var_os("IRIS_CATCHUP_REPAIR_BIN")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_htree").into()),
    )
    .unwrap();
    let base = command(&temp);
    let mut repair = Command::new(&repair_bin);
    repair.args(base.get_args());
    for (key, value) in base.get_envs() {
        if let Some(value) = value {
            repair.env(key, value);
        } else {
            repair.env_remove(key);
        }
    }
    repair
        .args([
            "nostr-index",
            "repair-id",
            "--root",
            before["root"].as_str().unwrap(),
            "--event",
        ])
        .arg(&event_file)
        .args([
            "--expected-id",
            &target.id.to_hex(),
            "--expected-event-sha256",
            &sha(&event_bytes),
            "--expected-checkpoint-sha256",
            &sha(&before_bytes),
            "--min-free-bytes",
            "0",
            "--receipt",
        ])
        .arg(&native_file);
    let native = success(output(repair).await);
    {
        use hashtree_cli::HashtreeStore;
        use hashtree_config::StorageBackend;
        use hashtree_core::{nhash_decode, Cid};
        let decode = |text: &str| {
            let value = nhash_decode(text).unwrap();
            Cid {
                hash: value.hash,
                key: value.decrypt_key,
            }
        };
        let backing = HashtreeStore::with_options_and_backend(
            temp.path().join("data"),
            None,
            1024 * 1024 * 1024,
            false,
            &StorageBackend::Lmdb,
        )
        .unwrap();
        let events = hashtree_nostr::NostrEventStore::new(backing.store_arc());
        let old = events
            .get_canonical_manifest(&decode(before["root"].as_str().unwrap()))
            .await
            .unwrap();
        let repaired = events
            .get_canonical_manifest(&decode(native["afterRoot"].as_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(old.metadata_cid, repaired.metadata_cid);
        assert_ne!(
            old.manifest.by_author_kind_time, repaired.manifest.by_author_kind_time,
            "wide real repair must exercise structural rewrite rejected by the old witness"
        );
        assert_ne!(old.manifest.by_tag, repaired.manifest.by_tag);
    }
    assert_eq!(
        checkpoint_bytes(&temp),
        before_bytes,
        "standalone repair must not rewrite coverage or checkpoint"
    );
    let mut after = before.clone();
    after["root"] = native["afterRoot"].clone();
    let mut after_bytes = serde_json::to_vec(&after).unwrap();
    after_bytes.push(b'\n');
    std::fs::write(
        temp.path().join("data/nostr-index/catchup-state.json"),
        &after_bytes,
    )
    .unwrap();
    let witness = json!({
        "format":"iris-social/archive-projection-repair@1", "completed":true,"exitCode":0,
        "nativeAuthoritySha256":"a".repeat(64),"nativeRunReceiptSha256":"b".repeat(64),
        "nativeRepairReceiptFile":native_file,"nativeRepairReceiptSha256":sha(&std::fs::read(&native_file).unwrap()),
        "tool":{"sourceCommit":"b8c165c26f39daf71e34cbfc48d1d69527d1f030", "binaryFile":repair_bin,
            "binarySha256":sha(&std::fs::read(&repair_bin).unwrap())},
        "before":{"state":before,"checkpointFile":before_file,"checkpointSha256":sha(&before_bytes)},
        "after":{"state":after,"checkpointSha256":sha(&after_bytes)},
        "originalAndPoolPinsUnchanged":true,"eventFile":event_file,"eventFileSha256":sha(&event_bytes),
        "event":target,"eventCid":native["eventCid"]
    });
    let witness_file = temp.path().join("repair-witness.json");
    let witness_bytes = serde_json::to_vec(&witness).unwrap();
    std::fs::write(&witness_file, &witness_bytes).unwrap();
    let mut absent = catchup(&temp, &root, &[&relay]);
    absent.args(["--max-authors-per-run", "1"]);
    assert!(!output(absent).await.status.success());
    assert_eq!(checkpoint_bytes(&temp), after_bytes);
    // Pinning an input does not turn malformed evidence into a valid repair.
    for field in ["checkpoint", "native-proof", "binary", "signature"] {
        let mut invalid = witness.clone();
        match field {
            "checkpoint" => invalid["after"]["checkpointSha256"] = json!("0".repeat(64)),
            "native-proof" => invalid["nativeRepairReceiptSha256"] = json!("0".repeat(64)),
            "binary" => invalid["tool"]["binarySha256"] = json!("0".repeat(64)),
            "signature" => invalid["event"]["content"] = json!("changed signed content"),
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&invalid).unwrap();
        std::fs::write(&witness_file, &bytes).unwrap();
        let mut cmd = catchup(&temp, &root, &[&relay]);
        cmd.arg("--repair-witness").arg(&witness_file).args([
            "--repair-witness-sha256",
            &sha(&bytes),
            "--max-authors-per-run",
            "1",
        ]);
        assert!(
            !output(cmd).await.status.success(),
            "accepted invalid {field}"
        );
        assert_eq!(checkpoint_bytes(&temp), after_bytes);
    }
    // Even an explicitly repinned witness cannot authorize a count/frontier change.
    let mut forged = witness.clone();
    forged["after"]["state"]["events_received"] = json!(10);
    let forged_bytes = serde_json::to_vec(&forged).unwrap();
    std::fs::write(&witness_file, &forged_bytes).unwrap();
    let mut invalid = catchup(&temp, &root, &[&relay]);
    invalid.arg("--repair-witness").arg(&witness_file).args([
        "--repair-witness-sha256",
        &sha(&forged_bytes),
        "--max-authors-per-run",
        "1",
    ]);
    assert!(!output(invalid).await.status.success());
    assert_eq!(checkpoint_bytes(&temp), after_bytes);
    std::fs::write(&witness_file, &witness_bytes).unwrap();
    relay.failing.store(true, Ordering::SeqCst);
    let mut failed_resume = catchup(&temp, &root, &[&relay]);
    failed_resume
        .arg("--repair-witness")
        .arg(&witness_file)
        .args([
            "--repair-witness-sha256",
            &sha(&witness_bytes),
            "--max-authors-per-run",
            "1",
        ]);
    assert!(!output(failed_resume).await.status.success());
    assert_eq!(
        checkpoint_bytes(&temp),
        after_bytes,
        "failed first resume must preserve exact witness-bound bytes"
    );
    relay.failing.store(false, Ordering::SeqCst);
    let mut resumed = catchup(&temp, &root, &[&relay]);
    resumed.arg("--repair-witness").arg(&witness_file).args([
        "--repair-witness-sha256",
        &sha(&witness_bytes),
        "--max-authors-per-run",
        "1",
    ]);
    let result = success(output(resumed).await);
    assert_eq!(result["next_author"], 2);
    assert_eq!(result["events_received"], 0);
    assert_eq!(result["root"], after["root"]);
    let saved = checkpoint(&temp);
    let head = saved["coverage_head"].as_str().unwrap();
    assert_ne!(head, before["coverage_head"].as_str().unwrap());
    let new_proof: Value = serde_json::from_slice(
        &std::fs::read(
            temp.path()
                .join(format!("data/nostr-index/catchup-coverage/{head}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(new_proof["before_root"], after["root"]);
    assert_eq!(new_proof["previous_head"], before["coverage_head"]);
    let mut later = catchup(&temp, &root, &[&relay]);
    later.arg("--repair-witness").arg(&witness_file).args([
        "--repair-witness-sha256",
        &sha(&witness_bytes),
        "--max-authors-per-run",
        "1",
    ]);
    assert_eq!(success(output(later).await)["next_author"], 3);
    assert_eq!(std::fs::read(&before_file).unwrap(), before_bytes);
    if !cfg!(debug_assertions) {
        // Fleet binds this real release-profile fixture result to both exact
        // executables; flags are emitted only after production witness resume.
        println!(
            "{}",
            json!({
                "format":"hashtree/native-repair-interop@2", "passed":true,
                "testName":"native_repair_witness_resumes_exact_coverage_once_then_becomes_inert",
                "releaseBinary":true, "legacyOrder64":true, "fixtureOrder":64,
                "fixtureEventCount":188, "fixtureTaggedNoteCount":181,
                "singleByIdInsertionVerified":true, "nonByIdMappingsUnchanged":true,
                "repairBinarySha256":witness["tool"]["binarySha256"],
                "successorBinarySha256":sha(&std::fs::read(env!("CARGO_BIN_EXE_htree")).unwrap())
            })
        );
    }
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn best_effort_accepts_an_existing_canonical_empty_archive() {
    let (temp, root, _, _) = damaged_note_fixture(usize::MAX).await;
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let fresh = event(&keys, 20, "first archived note");
    let relay = Relay::new(vec![fresh.clone()], false).await;
    let mut cmd = catchup(&temp, &root, &[&relay]);
    cmd.args(["--max-authors-per-run", "1"]);
    let result = success(output(cmd).await);
    assert_eq!(result["next_author"], 1);
    assert_eq!(result["events_received"], 1);
    let mut cmd = command(&temp);
    cmd.args([
        "nostr-index",
        "query",
        "--root",
        result["root"].as_str().unwrap(),
        "--filter",
        &json!({"ids":[fresh.id.to_hex()]}).to_string(),
        "--limit",
        "1",
    ]);
    let found = success(output(cmd).await);
    assert_eq!(found["count"], 1);
    assert_eq!(found["events"][0]["id"], fresh.id.to_hex());
}

#[cfg(feature = "lmdb")]
async fn bounded_note_fixture(missing_body: bool) -> (TempDir, String, Vec<String>, Event) {
    use hashtree_cli::HashtreeStore;
    use hashtree_config::StorageBackend;
    use hashtree_core::{nhash_decode, nhash_encode_full, Cid, NHashData};
    use hashtree_index::{BTree, BTreeOptions};
    use hashtree_nostr::{NostrEventIndex, NostrEventStore};
    let temp = TempDir::new().unwrap();
    let (authors, mut old, _, _) = fixture();
    let unsampled = old[0].clone();
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    for i in 0..300 {
        old.push(event(&keys, 1000 + i, "prolific retained history"));
    }
    let selected = old[old.len() - 3..]
        .iter()
        .chain(std::iter::once(&old[1]))
        .map(|event| event.id.to_hex())
        .collect::<Vec<_>>();
    let original = seed(&temp, &old, &authors).await;
    let decoded = nhash_decode(&original).unwrap();
    let original = Cid {
        hash: decoded.hash,
        key: decoded.decrypt_key,
    };
    let backing = HashtreeStore::with_options_and_backend(
        temp.path().join("data"),
        None,
        1024 * 1024 * 1024,
        false,
        &StorageBackend::Lmdb,
    )
    .unwrap();
    let events = NostrEventStore::new(backing.store_arc());
    let index = BTree::new(backing.store_arc(), BTreeOptions::default());
    let tree =
        hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(backing.store_arc()));
    let entries = tree.list_directory_required(&original).await.unwrap();
    let mut roots = NostrEventIndex::ALL
        .into_iter()
        .map(|name| {
            (
                name,
                entries
                    .iter()
                    .find(|entry| entry.name == name.name())
                    .map(|entry| Cid {
                        hash: entry.hash,
                        key: entry.key,
                    }),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let target_body = index
        .get_link(roots[&NostrEventIndex::ById].as_ref(), &selected[0])
        .await
        .unwrap()
        .unwrap();
    let missing = selected
        .iter()
        .cloned()
        .chain(std::iter::once(unsampled.id.to_hex()));
    roots.insert(
        NostrEventIndex::ById,
        index
            .update_links(
                roots[&NostrEventIndex::ById].as_ref(),
                missing.map(|id| (id, None)),
            )
            .await
            .unwrap(),
    );
    let damaged = events
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    if missing_body {
        use hashtree_core::Store;
        backing.store_arc().delete(&target_body.hash).await.unwrap();
    }
    backing.force_sync().unwrap();
    let root = nhash_encode_full(&NHashData {
        hash: damaged.hash,
        decrypt_key: damaged.key,
    })
    .unwrap();
    (temp, root, selected, unsampled)
}

#[cfg(feature = "lmdb")]
fn bounded_catchup(temp: &TempDir, root: &str, relay: &Relay, since: &str, until: &str) -> Command {
    let mut cmd = command(temp);
    cmd.args(["nostr-index", "catch-up", "--root", root, "--authors-file"])
        .arg(temp.path().join("authors.txt"))
        .args([
            "--source-mode",
            "best-effort",
            "--min-free-bytes",
            "0",
            "--since",
            since,
            "--until",
            until,
            "--kind",
            "1",
            "--kind",
            "5",
            "--page-size",
            "4",
            "--fetch-timeout-secs",
            "2",
            "--relay",
            &relay.url,
        ]);
    cmd
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn catchup_bounds_prolific_history_and_advances_other_authors_with_exact_retention() {
    for refetch_old in [false, true] {
        let (temp, root, selected, unsampled) = bounded_note_fixture(false).await;
        let relay = Relay::new(
            if refetch_old {
                vec![unsampled.clone()]
            } else {
                Vec::new()
            },
            false,
        )
        .await;
        // The empty-source case is >24h after every retained note; no date-based
        // cutoff can satisfy the old-root newest-three retention contract.
        let (since, until) = if refetch_old {
            ("0", "5")
        } else {
            ("1000000", "1000100")
        };
        let run_output = output(bounded_catchup(&temp, &root, &relay, since, until)).await;
        let stderr = String::from_utf8_lossy(&run_output.stderr).to_string();
        let scanned = stderr
            .lines()
            .filter(|line| line.starts_with("Nostr catchup checkpoint:"))
            .map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("ids_scanned="))
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            scanned,
            vec![if refetch_old { 4 } else { 3 }, 1, 1, 1, 1],
            "{stderr}"
        );
        let result = success(run_output);
        assert_eq!(result["next_author"], 5);
        assert_eq!(result["events_received"], u64::from(refetch_old));
        let proofs = coverage_chain(&temp);
        assert_eq!(proofs.len(), 5);
        assert_eq!(
            proofs
                .iter()
                .map(|proof| proof["events_received"].as_u64().unwrap())
                .sum::<u64>(),
            u64::from(refetch_old)
        );
        let mut query = command(&temp);
        let wanted = selected
            .iter()
            .cloned()
            .chain(std::iter::once(unsampled.id.to_hex()))
            .collect::<Vec<_>>();
        query.args([
            "nostr-index",
            "query",
            "--root",
            result["root"].as_str().unwrap(),
            "--filter",
            &json!({"ids":wanted}).to_string(),
            "--limit",
            "5",
        ]);
        let found = success(output(query).await);
        assert_eq!(found["count"], if refetch_old { 5 } else { 4 });
        let found = found["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(selected.iter().all(|id| found.contains(id.as_str())));
        assert_eq!(found.contains(unsampled.id.to_hex().as_str()), refetch_old);
    }
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn catchup_selected_missing_body_preserves_author_frontier_and_old_root() {
    let (temp, root, _, _) = bounded_note_fixture(true).await;
    let relay = Relay::new(Vec::new(), false).await;
    let result = output(bounded_catchup(&temp, &root, &relay, "1000000", "1000100")).await;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("reconcile retained note IDs"));
    let saved = checkpoint(&temp);
    assert_eq!(saved["next_author"], 0);
    assert_eq!(saved["root"], root);
    assert_eq!(saved["events_received"], 0);
    assert!(saved["coverage_head"].is_null());
}
