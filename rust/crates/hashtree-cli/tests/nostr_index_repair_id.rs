#![cfg(feature = "lmdb")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};

use hashtree_cli::HashtreeStore;
use hashtree_config::StorageBackend;
use hashtree_core::{nhash_encode_full, Cid, HashTree, HashTreeConfig, NHashData, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::catchup::{CatchupPolicy, CatchupState};
use hashtree_nostr::{NostrEventIndex, NostrEventStore, StoredNostrEvent, VerifiedEvent};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn root_text(cid: &Cid) -> String {
    nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .unwrap()
}

fn note(keys: &Keys, at: u64) -> StoredNostrEvent {
    VerifiedEvent::try_from(
        EventBuilder::new(Kind::TextNote, format!("synthetic archive {at}"))
            .custom_created_at(Timestamp::from_secs(at))
            .tags([Tag::parse(["t", "retained-fixture"]).unwrap()])
            .sign_with_keys(keys)
            .unwrap(),
    )
    .unwrap()
    .to_stored_event()
    .into_stored()
}

struct Fixture {
    temp: TempDir,
    root: Cid,
    original: Cid,
    event: StoredNostrEvent,
    event_cid: Cid,
    tag_root: Cid,
    checkpoint: Vec<u8>,
    input: Vec<u8>,
}

impl Fixture {
    fn store(&self) -> HashtreeStore {
        HashtreeStore::with_options_and_backend(
            self.temp.path().join("data"),
            None,
            1024 * 1024 * 1024,
            false,
            &StorageBackend::Lmdb,
        )
        .unwrap()
    }

    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(data.join("nostr-index")).unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        std::fs::write(
            temp.path().join("config/config.toml"),
            "[storage]\nmax_size_gb = 1\nevict_orphans = false\n",
        )
        .unwrap();
        let keys = Keys::parse(&format!("{:064x}", 3)).unwrap();
        let events = (1..=48).map(|at| note(&keys, at)).collect::<Vec<_>>();
        let event = events[23].clone();
        let store = HashtreeStore::with_options_and_backend(
            &data,
            None,
            1024 * 1024 * 1024,
            false,
            &StorageBackend::Lmdb,
        )
        .unwrap();
        let backing = store.store_arc();
        let event_store = NostrEventStore::new(backing.clone());
        let original = event_store.build(None, events).await.unwrap().unwrap();
        let manifest = event_store.get_manifest(Some(&original)).await.unwrap();
        let index = BTree::new(backing.clone(), BTreeOptions::default());
        let event_cid = index
            .get_link(manifest.by_id.as_ref(), &event.id)
            .await
            .unwrap()
            .unwrap();
        let tree = HashTree::new(HashTreeConfig::new(backing));
        let entries = tree.list_directory_required(&original).await.unwrap();
        let mut roots = NostrEventIndex::ALL
            .into_iter()
            .map(|index| {
                (
                    index,
                    entries
                        .iter()
                        .find(|entry| entry.name == index.name())
                        .map(|entry| Cid {
                            hash: entry.hash,
                            key: entry.key,
                        }),
                )
            })
            .collect::<BTreeMap<_, _>>();
        roots.insert(
            NostrEventIndex::ById,
            index
                .update_links(manifest.by_id.as_ref(), [(event.id.clone(), None)])
                .await
                .unwrap(),
        );
        let damaged = event_store
            .write_bulk_index_manifest(&roots)
            .await
            .unwrap()
            .unwrap();
        // A later ordinary append preserves the omission. The complete repaired
        // by-ID path has never existed, so the failure test below observes real
        // writes before encountering its missing later projection.
        let root = event_store
            .build_with_superseded_nodes(Some(&damaged), [note(&keys, 500)])
            .await
            .unwrap()
            .root
            .unwrap();
        let manifest = event_store.get_manifest(Some(&root)).await.unwrap();
        store.force_sync().unwrap();
        drop(tree);
        drop(index);
        drop(event_store);
        drop(store);
        let state = CatchupState {
            version: 2,
            coverage_head: None,
            policy: CatchupPolicy {
                base_root: root_text(&original),
                source_mode: Default::default(),
                authors_sha256: hash(keys.public_key().to_hex().as_bytes()),
                author_count: 2,
                initial_since: 100,
                overlap_secs: 86400,
                relays: vec!["wss://unused.invalid".into()],
                pubsub_peers: Vec::new(),
                kinds: vec![1, 5],
                page_size: 1000,
                max_pages_per_author: 10000,
                max_events_per_author: 65536,
                max_bytes_per_author: 67108864,
                fetch_timeout_secs: 30,
                index_commit_batch_size: 256,
            },
            root: root_text(&root),
            pass_since: 100,
            pass_until: 1000,
            next_author: 1,
            events_received: 7,
        };
        let mut checkpoint = serde_json::to_vec(&state).unwrap();
        checkpoint.push(b'\n');
        std::fs::write(data.join("nostr-index/catchup-state.json"), &checkpoint).unwrap();
        std::fs::write(
            data.join("nostr-index/latest-root.txt"),
            "untouched latest root\n",
        )
        .unwrap();
        std::fs::write(
            data.join("nostr-index/crawl-state.json"),
            "untouched original crawl\n",
        )
        .unwrap();
        let input = serde_json::to_vec(&event).unwrap();
        std::fs::write(temp.path().join("event.json"), &input).unwrap();
        Self {
            temp,
            root,
            original,
            event,
            event_cid,
            tag_root: manifest.by_tag.unwrap(),
            checkpoint,
            input,
        }
    }

    fn receipt(&self) -> PathBuf {
        self.temp.path().join("repair.json")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_htree"));
        cmd.arg("--data-dir")
            .arg(self.temp.path().join("data"))
            .env("HTREE_CONFIG_DIR", self.temp.path().join("config"))
            .env("HTREE_DATA_DIR", self.temp.path().join("data"))
            .env("HOME", self.temp.path())
            .env("TOKIO_WORKER_THREADS", "2")
            .env_remove("NOSTR_SECRET_KEY")
            .env_remove("NOSTR_PRIVATE_KEY")
            .env_remove("NOSTR_KEY");
        cmd
    }

    fn repair(
        &self,
        root: &Cid,
        id: &str,
        event_sha: &str,
        checkpoint_sha: &str,
        floor: u64,
    ) -> Command {
        let mut cmd = self.command();
        cmd.args([
            "nostr-index",
            "repair-id",
            "--root",
            &root_text(root),
            "--event",
        ])
        .arg(self.temp.path().join("event.json"))
        .args([
            "--expected-id",
            id,
            "--expected-event-sha256",
            event_sha,
            "--expected-checkpoint-sha256",
            checkpoint_sha,
            "--min-free-bytes",
            &floor.to_string(),
            "--receipt",
        ])
        .arg(self.receipt());
        cmd
    }

    fn repair_default(&self) -> Command {
        self.repair(
            &self.root,
            &self.event.id,
            &hash(&self.input),
            &hash(&self.checkpoint),
            0,
        )
    }

    fn unchanged_state(&self) {
        let index = self.temp.path().join("data/nostr-index");
        assert_eq!(
            std::fs::read(index.join("catchup-state.json")).unwrap(),
            self.checkpoint
        );
        assert_eq!(
            std::fs::read_to_string(index.join("latest-root.txt")).unwrap(),
            "untouched latest root\n"
        );
        assert_eq!(
            std::fs::read_to_string(index.join("crawl-state.json")).unwrap(),
            "untouched original crawl\n"
        );
    }

    async fn query(&self, root: &str) -> Value {
        let mut cmd = self.command();
        cmd.args([
            "nostr-index",
            "query",
            "--root",
            root,
            "--filter",
            &json!({"ids": [&self.event.id]}).to_string(),
        ]);
        let output = run(cmd).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

async fn run(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn cli_repairs_one_proven_id_without_advancing_any_checkpoint() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.query(&root_text(&fixture.root)).await["count"], 0);
    let output = run(fixture.repair_default()).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(fixture.receipt()).unwrap()).unwrap();
    assert_eq!(
        receipt,
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    );
    assert_eq!(receipt["format"], "nostr-index/id-repair@1");
    assert_eq!(receipt["status"], "complete");
    assert_eq!(receipt["beforeRoot"], root_text(&fixture.root));
    assert_eq!(receipt["checkpointSha256"], hash(&fixture.checkpoint));
    assert_eq!(receipt["eventFileSha256"], hash(&fixture.input));
    assert_eq!(receipt["eventCid"], root_text(&fixture.event_cid));
    assert_eq!(
        receipt["event"],
        json!({"id": fixture.event.id, "pubkey": fixture.event.pubkey, "kind": 1, "created_at": fixture.event.created_at})
    );
    assert_eq!(receipt["absentByIdBefore"], true);
    assert_eq!(receipt["verifiedAfter"], true);
    assert_eq!(
        fixture.query(receipt["afterRoot"].as_str().unwrap()).await["count"],
        1
    );
    assert_eq!(fixture.query(&root_text(&fixture.root)).await["count"], 0);
    assert_eq!(
        fixture.query(&root_text(&fixture.original)).await["count"],
        1
    );
    fixture.unchanged_state();
    let immutable = std::fs::read(fixture.receipt()).unwrap();
    assert!(!run(fixture.repair_default()).await.status.success());
    assert_eq!(std::fs::read(fixture.receipt()).unwrap(), immutable);
}

#[tokio::test]
async fn cli_repair_rejects_wrong_pins_signature_root_and_floor_without_receipt() {
    let fixture = Fixture::new().await;
    let zeros = "0".repeat(64);
    let event_sha = hash(&fixture.input);
    let checkpoint_sha = hash(&fixture.checkpoint);
    let cases = [
        (
            fixture.repair(&fixture.root, &zeros, &event_sha, &checkpoint_sha, 0),
            "expected ID",
        ),
        (
            fixture.repair(
                &fixture.original,
                &fixture.event.id,
                &event_sha,
                &checkpoint_sha,
                0,
            ),
            "checkpoint root",
        ),
        (
            fixture.repair(&fixture.root, &fixture.event.id, &zeros, &checkpoint_sha, 0),
            "event file SHA-256",
        ),
        (
            fixture.repair(&fixture.root, &fixture.event.id, &event_sha, &zeros, 0),
            "checkpoint SHA-256",
        ),
        (
            fixture.repair(
                &fixture.root,
                &fixture.event.id,
                &event_sha,
                &checkpoint_sha,
                u64::MAX,
            ),
            "physical-space",
        ),
    ];
    for (cmd, reason) in cases {
        let output = run(cmd).await;
        assert!(!output.status.success(), "{reason}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.receipt().exists());
        fixture.unchanged_state();
    }
    let mut invalid = fixture.event.clone();
    invalid.sig = "0".repeat(128);
    let bytes = serde_json::to_vec(&invalid).unwrap();
    std::fs::write(fixture.temp.path().join("event.json"), &bytes).unwrap();
    let output = run(fixture.repair(
        &fixture.root,
        &fixture.event.id,
        &hash(&bytes),
        &checkpoint_sha,
        0,
    ))
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("verify original signed event"));
    assert!(!fixture.receipt().exists());
    fixture.unchanged_state();

    // A valid signature is insufficient: this is a projection repair, not an
    // arbitrary import path. The original signed body must already be linked.
    let unrelated = note(&Keys::parse(&format!("{:064x}", 4)).unwrap(), 900);
    let bytes = serde_json::to_vec(&unrelated).unwrap();
    std::fs::write(fixture.temp.path().join("event.json"), &bytes).unwrap();
    let output = run(fixture.repair(
        &fixture.root,
        &unrelated.id,
        &hash(&bytes),
        &checkpoint_sha,
        0,
    ))
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("not reachable through exact author-kind-time key"));
    assert!(!fixture.receipt().exists());
    fixture.unchanged_state();

    let oversized = vec![b' '; 1024 * 1024 + 1];
    std::fs::write(fixture.temp.path().join("event.json"), &oversized).unwrap();
    let output = run(fixture.repair(
        &fixture.root,
        &fixture.event.id,
        &hash(&oversized),
        &checkpoint_sha,
        0,
    ))
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds 1 MiB"));
    assert!(!fixture.receipt().exists());
    fixture.unchanged_state();
}

#[tokio::test]
async fn cli_partial_index_write_failure_keeps_checkpoint_and_emits_no_receipt() {
    let fixture = Fixture::new().await;
    let before_count = {
        let store = fixture.store();
        let backing = store.store_arc();
        assert!(backing.delete(&fixture.tag_root.hash).await.unwrap());
        store.force_sync().unwrap();
        backing.stats().unwrap().count
    };
    let output = run(fixture.repair_default()).await;
    assert!(
        !output.status.success(),
        "missing old tag subtree must fail closed"
    );
    assert!(!fixture.receipt().exists());
    fixture.unchanged_state();
    assert_eq!(fixture.query(&root_text(&fixture.root)).await["count"], 0);
    assert_eq!(
        fixture.query(&root_text(&fixture.original)).await["count"],
        1
    );
    let store = fixture.store();
    let backing = store.store_arc();
    assert!(
        backing.stats().unwrap().count > before_count,
        "failure occurs after a copy-on-write by-ID path was written"
    );
    assert!(backing
        .get(&fixture.event_cid.hash)
        .await
        .unwrap()
        .is_some());
}
