use super::*;
use hashtree_nostr::{
    catchup::{
        CatchupPolicy, CatchupSourceCoverage, CatchupSourceMode, CatchupSourceStatus, CatchupState,
    },
    stored_event_from_nostr_sdk_event, StoredNostrEvent,
};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{EventBuilder, Keys, Kind, Timestamp, SECP256K1};

fn event(kind: u16, at: u64, signer: u64) -> StoredNostrEvent {
    let key = Keys::parse(&format!("{signer:064x}")).unwrap();
    stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::from(kind), format!("fixture-{kind}-{at}"))
            .custom_created_at(Timestamp::from_secs(at))
            .build(key.public_key())
            .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(at), &key)
            .unwrap(),
    )
}
fn batch(events: &[StoredNostrEvent]) -> Vec<u8> {
    coverage::canonical(&serde_json::json!({
        "format":"iris-social/archive-forward-batch@1", "configSha256":"c".repeat(64),
        "since":10, "events":events,
    }))
    .unwrap()
    .into_bytes()
}
fn storage(path: &Path) -> HashtreeStore {
    HashtreeStore::with_options_and_backend(
        path,
        None,
        1024 * 1024 * 1024,
        false,
        &hashtree_config::StorageBackend::Fs,
    )
    .unwrap()
}
struct Fixture {
    temp: tempfile::TempDir,
    store: HashtreeStore,
    directory: PathBuf,
    authors: Vec<u8>,
    before: CatchupState,
    events: Vec<StoredNostrEvent>,
    bytes: Vec<u8>,
    args: AppendForwardArgs,
}
impl Fixture {
    async fn new(events: Vec<StoredNostrEvent>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = storage(&temp.path().join("store"));
        let directory = temp.path().join("checkpoint");
        std::fs::create_dir(&directory).unwrap();
        let old = event(1, 1, 1);
        let root = NostrEventStore::new(store.store_arc())
            .build(None, [old.clone()])
            .await
            .unwrap()
            .unwrap();
        store.force_sync().unwrap();
        let authors = format!("{}\n", old.pubkey).into_bytes();
        let initial = CatchupState {
            version: 2,
            root: cid_to_nhash(&root).unwrap(),
            policy: CatchupPolicy {
                base_root: cid_to_nhash(&root).unwrap(),
                authors_sha256: coverage::sha(&authors),
                author_count: 1,
                initial_since: 10,
                overlap_secs: 10,
                relays: vec!["ws://fixture.invalid".into()],
                source_mode: CatchupSourceMode::Strict,
                kinds: vec![1, 5],
                page_size: 4,
                max_pages_per_author: 10,
                max_events_per_author: 100,
                max_bytes_per_author: 65536,
                fetch_timeout_secs: 1,
                index_commit_batch_size: 4,
            },
            pass_since: 10,
            pass_until: 100,
            next_author: 0,
            events_received: 0,
            coverage_head: None,
        };
        let mut before = initial.clone();
        before.next_author = 1;
        before = coverage::commit_with_pubsub(
            &store,
            &directory.join("catchup-coverage"),
            &directory.join("catchup-state.json"),
            &initial,
            before,
            &old.pubkey,
            vec![CatchupSourceCoverage {
                relay: "ws://fixture.invalid".into(),
                status: CatchupSourceStatus::Complete,
                error: None,
            }],
            None,
        )
        .unwrap();
        let bytes = batch(&events);
        let args = AppendForwardArgs {
            batch: temp.path().join("batch.json"),
            batch_sha256: coverage::sha(&bytes),
            config_sha256: "c".repeat(64),
            since: 10,
            graph_authors_file: None,
            graph_authors_sha256: None,
            authors_file: temp.path().join("authors.txt"),
            min_free_bytes: 0,
        };
        std::fs::write(&args.authors_file, &authors).unwrap();
        std::fs::write(&args.batch, &bytes).unwrap();
        Self {
            temp,
            store,
            directory,
            authors,
            before,
            events,
            bytes,
            args,
        }
    }
    async fn append(&self) -> Result<Vec<u8>> {
        append(
            &self.store,
            &self.directory,
            &self.bytes,
            &self.events,
            &self.authors,
            &self.args,
        )
        .await
    }
}

#[test]
fn batch_authenticates_exact_bytes_signatures_kinds_and_bounds() {
    let good = event(21, 20, 2);
    let bytes = batch(&[good.clone()]);
    let validate =
        |bytes: &[u8]| input::validate(bytes, &coverage::sha(bytes), &"c".repeat(64), 10);
    assert_eq!(validate(&bytes).unwrap().events, [good.clone()]);
    assert!(input::validate(&bytes, &"a".repeat(64), &"c".repeat(64), 10).is_err());
    assert!(input::validate(&bytes, &coverage::sha(&bytes), &"d".repeat(64), 10).is_err());
    assert!(input::validate(&bytes, &coverage::sha(&bytes), &"c".repeat(64), 11).is_err());
    let mut newline = bytes.clone();
    newline.push(b'\n');
    assert!(validate(&newline).is_err());
    let mut bad = good.clone();
    bad.content.push('!');
    assert!(validate(&batch(&[bad])).is_err());
    let mut bad = good.clone();
    bad.sig = "0".repeat(128);
    assert!(validate(&batch(&[bad])).is_err());
    assert!(validate(&batch(&[event(1, 20, 2)])).is_err());
    assert!(validate(&batch(&[event(21, 9, 2)])).is_err());
    assert!(validate(&batch(&[])).is_err());
    assert!(validate(&batch(&vec![good.clone(); 33])).is_err());
    assert!(validate(&batch(&[good.clone(), good.clone()])).is_err());
    let mut oversized = good;
    oversized.content = "x".repeat(128 * 1024);
    assert!(validate(&batch(&[oversized])).is_err());
    assert!(validate(&vec![b' '; input::MAX_BATCH + 1]).is_err());
}

fn graph_batch(events: &[StoredNostrEvent], authors: &[u8]) -> Vec<u8> {
    coverage::canonical(&serde_json::json!({
        "format": "iris-social/archive-forward-batch@2", "configSha256": "c".repeat(64),
        "graphAuthorsSha256": coverage::sha(authors), "since": 10, "events": events,
    }))
    .unwrap()
    .into_bytes()
}

#[tokio::test]
async fn graph_batch_validates_authors_and_indexes_regular_notes_and_replaceables() {
    let note = event(1, 20, 2);
    let authors = format!("{}\t1\n", note.pubkey);
    let authors = authors.as_bytes();
    let validate =
        |bytes: &[u8]| input::validate(bytes, &coverage::sha(bytes), &"c".repeat(64), 10);
    let listing = event(30402, 20, 3);
    let events = vec![
        note.clone(),
        listing,
        event(0, 21, 2),
        event(0, 22, 2),
        event(30023, 22, 2),
    ];
    let bytes = graph_batch(&events, authors);
    let validated = validate(&bytes).unwrap();
    input::verify_graph_authors(&validated, authors, &coverage::sha(authors)).unwrap();
    assert!(input::verify_graph_authors(&validated, b"wrong", &coverage::sha(authors)).is_err());
    let outside = validate(&graph_batch(&[event(1, 20, 3)], authors)).unwrap();
    assert!(input::verify_graph_authors(&outside, authors, &coverage::sha(authors)).is_err());
    for kind in [4, 1059, 1234, 7375, 20_001, 30003, 31234] {
        assert!(validate(&graph_batch(&[event(kind, 20, 2)], authors)).is_err());
    }

    let mut fixture = Fixture::new(events).await;
    fixture.args.batch_sha256 = coverage::sha(&bytes);
    fixture.bytes = bytes;
    let receipt: receipt::Receipt =
        serde_json::from_slice(&fixture.append().await.unwrap()).unwrap();
    assert_eq!(receipt.events[0].retained_id, note.id);
    assert_eq!(receipt.events[2].status, "superseded");
    assert_eq!(receipt.events[2].retained_id, fixture.events[3].id);
}

#[tokio::test]
async fn preserves_history_and_authenticates_replaceable_dispositions() {
    let mut events = input::KINDS
        .iter()
        .map(|kind| event(*kind as u16, 20, 2))
        .collect::<Vec<_>>();
    events.push(event(30402, 21, 2));
    // Equal timestamps use the index's lexicographically lower-ID winner.
    let key = Keys::parse(&format!("{:064x}", 2)).unwrap();
    events.push(stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::from(30402), "tie")
            .custom_created_at(Timestamp::from_secs(21))
            .build(key.public_key())
            .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(99), &key)
            .unwrap(),
    ));
    let f = Fixture::new(events).await;
    let bytes = f.append().await.unwrap();
    let receipt: receipt::Receipt = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(receipt.counts.input, 13);
    assert_eq!(receipt.counts.superseded, 2);
    let mut unchanged = receipt.after.clone();
    unchanged.root = f.before.root.clone();
    unchanged.coverage_head = f.before.coverage_head.clone();
    assert_eq!(unchanged, f.before);
    journal::verify(&f.store, &f.directory, &receipt, &f.authors)
        .await
        .unwrap();
    let writer = NostrEventStore::new(f.store.store_arc());
    let old_root = parse_root_text(&f.before.root).unwrap();
    assert!(writer
        .get_verified_by_id(Some(&old_root), &event(1, 1, 1).id)
        .await
        .unwrap()
        .is_some());
    assert!(writer
        .get_by_id(Some(&old_root), &f.events[0].id)
        .await
        .unwrap()
        .is_none());
    assert!(!journal::present(&f.directory.join(journal::PENDING)).unwrap());
}

#[tokio::test]
async fn refuses_pending_historical_batch_without_touching_checkpoint() {
    let f = Fixture::new(vec![event(21, 20, 2)]).await;
    std::fs::write(f.directory.join("catchup-append.json"), b"protected cursor").unwrap();
    assert!(f.append().await.is_err());
    assert_eq!(journal::state(&f.directory).unwrap(), f.before);
    assert_eq!(
        std::fs::read(f.directory.join("catchup-append.json")).unwrap(),
        b"protected cursor"
    );
    assert!(
        !journal::present(&journal::batch_path(&f.directory, &f.args.batch_sha256).unwrap())
            .unwrap()
    );
}

#[tokio::test]
async fn recovers_reopened_storage_at_each_durable_commit_boundary() {
    for stage in ["pending", "checkpoint", "receipt", "retirement"] {
        let f = Fixture::new(vec![event(21, 20, 2)]).await;
        let blocked = match stage {
            "pending" => f.directory.join(".catchup-forward-pending.json.tmp"),
            "checkpoint" => f.directory.join(".catchup-state.json.tmp"),
            "receipt" => journal::receipt_path(&f.directory, &f.args.batch_sha256).unwrap(),
            _ => f.directory.join("unused"),
        };
        if stage != "retirement" {
            std::fs::create_dir_all(&blocked).unwrap();
            assert!(f.append().await.is_err(), "{stage}");
            std::fs::remove_dir(&blocked).unwrap();
        } else {
            let bytes = f.append().await.unwrap();
            // Durable receipt with the still-present pending journal is the
            // on-disk state of a crash immediately before journal retirement.
            std::fs::write(f.directory.join(journal::PENDING), bytes).unwrap();
        }
        if stage == "pending" || stage == "checkpoint" {
            assert_eq!(journal::state(&f.directory).unwrap(), f.before);
        }
        drop(f.store);
        let reopened = storage(&f.temp.path().join("store"));
        journal::recover(&reopened, &f.directory, &f.authors)
            .await
            .unwrap();
        if stage == "pending" {
            append(
                &reopened,
                &f.directory,
                &f.bytes,
                &f.events,
                &f.authors,
                &f.args,
            )
            .await
            .unwrap();
        }
        let bytes =
            coverage::read(&journal::receipt_path(&f.directory, &f.args.batch_sha256).unwrap())
                .unwrap();
        let receipt: receipt::Receipt = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(journal::state(&f.directory).unwrap(), receipt.after);
        journal::verify(&reopened, &f.directory, &receipt, &f.authors)
            .await
            .unwrap();
        assert!(!journal::present(&f.directory.join(journal::PENDING)).unwrap());
    }
}

#[tokio::test]
async fn corrupt_pending_or_changed_frontier_cannot_replace_checkpoint() {
    for corrupt_receipt in [false, true] {
        let f = Fixture::new(vec![event(21, 20, 2)]).await;
        let blocked = f.directory.join(".catchup-state.json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(f.append().await.is_err());
        std::fs::remove_dir(&blocked).unwrap();
        if corrupt_receipt {
            let path = f.directory.join(journal::PENDING);
            let mut receipt: receipt::Receipt =
                serde_json::from_slice(&coverage::read(&path).unwrap()).unwrap();
            receipt.events[0].retained_id = "a".repeat(64);
            std::fs::write(&path, receipt.bytes().unwrap()).unwrap();
        } else {
            let mut changed = f.before.clone();
            changed.events_received += 1;
            persist_json_atomic(&f.directory.join("catchup-state.json"), &changed, "fixture")
                .unwrap();
        }
        let before = coverage::read(&f.directory.join("catchup-state.json")).unwrap();
        assert!(journal::recover(&f.store, &f.directory, &f.authors)
            .await
            .is_err());
        assert_eq!(
            coverage::read(&f.directory.join("catchup-state.json")).unwrap(),
            before
        );
        assert!(journal::present(&f.directory.join(journal::PENDING)).unwrap());
    }
}

#[tokio::test]
async fn bridge_collapses_across_forward_batches_and_saved_rollover() {
    let f = Fixture::new(vec![event(21, 20, 2)]).await;
    let first: receipt::Receipt = serde_json::from_slice(&f.append().await.unwrap()).unwrap();
    let rolled = CatchupState::prepare(
        Some(first.after.clone()),
        f.before.policy.clone(),
        Some(200),
        200,
    )
    .unwrap();
    persist_json_atomic(&f.directory.join("catchup-state.json"), &rolled, "fixture").unwrap();
    let authors = input::authors(&f.authors, &rolled).unwrap();
    coverage::validate_head(&f.directory.join("catchup-coverage"), &rolled, &authors).unwrap();
    let bridge = bridge::Bridge::new(
        &f.directory.join("catchup-coverage"),
        &rolled,
        rolled.root.clone(),
        "d".repeat(64),
        &authors,
    )
    .unwrap();
    assert_eq!(bridge.historical_state.root, f.before.root);
    assert_eq!(
        bridge.historical_state.coverage_head,
        f.before.coverage_head
    );
    assert_eq!(bridge.historical_state.pass_until, 200);
    bridge
        .validate(
            &f.directory.join("catchup-coverage"),
            &bridge.after().unwrap(),
            &authors,
        )
        .unwrap();
    let mut forged = bridge.after().unwrap();
    forged.next_author = 1;
    assert!(bridge
        .validate(&f.directory.join("catchup-coverage"), &forged, &authors)
        .is_err());
}

#[tokio::test]
async fn replacement_uses_existing_first_nonempty_d_tag_semantics() {
    let key = Keys::parse(&format!("{:064x}", 2)).unwrap();
    let mut events = Vec::new();
    for (at, tags) in [
        (20, vec![vec!["d", "listing"]]),
        (21, vec![vec!["d", ""], vec!["d", "listing"]]),
    ] {
        let tags = tags.into_iter().map(|tag| nostr::Tag::parse(tag).unwrap());
        events.push(stored_event_from_nostr_sdk_event(
            &EventBuilder::new(Kind::from(30402), "listing")
                .tags(tags)
                .custom_created_at(Timestamp::from_secs(at))
                .build(key.public_key())
                .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(at), &key)
                .unwrap(),
        ));
    }
    let f = Fixture::new(events).await;
    let receipt: receipt::Receipt = serde_json::from_slice(&f.append().await.unwrap()).unwrap();
    assert_eq!(receipt.events[0].status, "superseded");
    assert_eq!(receipt.events[0].retained_id, f.events[1].id);
    assert_eq!(receipt.events[1].status, "indexed");
}
