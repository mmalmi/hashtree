use super::*;
use hashtree_core::MemoryStore;
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStoreOptions};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use std::path::PathBuf;

fn event(id: &str, created_at: u64, kind: u32) -> StoredNostrEvent {
    StoredNostrEvent {
        id: id.to_string(),
        pubkey: "11".repeat(32),
        created_at,
        kind,
        tags: vec![vec!["t".to_string(), "Rust".to_string()]],
        content: id.to_string(),
        sig: "22".repeat(64),
    }
}

#[tokio::test]
async fn spool_replaces_old_events_and_bulk_builds_final_indexes_once() {
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let mut old = event(&"01".repeat(32), 10, 30_000);
    let mut new = event(&"02".repeat(32), 20, 30_000);
    let long_identifier = "d".repeat(1_024);
    old.tags
        .push(vec!["d".to_string(), long_identifier.clone()]);
    new.tags.push(vec!["d".to_string(), long_identifier]);
    let note = event(&"03".repeat(32), 15, 1);
    let old_cid = Cid::public([1; 32]);
    let new_cid = Cid::public([2; 32]);
    let note_cid = Cid::public([3; 32]);

    spool.apply(vec![(old, old_cid)]).unwrap();
    let report = spool
        .apply(vec![
            (note.clone(), note_cid.clone()),
            (new.clone(), new_cid.clone()),
        ])
        .unwrap();
    assert_eq!(report.replaced, 1);

    let store = Arc::new(MemoryStore::new());
    let by_id = spool
        .build_index_root(NostrEventIndex::ById, Arc::clone(&store), 8)
        .await
        .unwrap()
        .unwrap();
    let replaceable = spool
        .build_index_root(
            NostrEventIndex::ParameterizedReplaceable,
            Arc::clone(&store),
            8,
        )
        .await
        .unwrap()
        .unwrap();
    let btree = BTree::new(store, BTreeOptions { order: Some(8) });

    assert_eq!(
        btree.count_stored_links(Some(&by_id)).await.unwrap(),
        Some(2)
    );
    assert_eq!(
        btree.get_link(Some(&by_id), &new.id).await.unwrap(),
        Some(new_cid)
    );
    assert_eq!(
        btree.get_link(Some(&by_id), &note.id).await.unwrap(),
        Some(note_cid)
    );
    let (_, slot) = nostr_replaceable_slot(&new).unwrap();
    assert_eq!(
        btree.get_link(Some(&replaceable), &slot).await.unwrap(),
        Some(Cid::public([2; 32]))
    );
}

#[test]
fn rejected_spool_edge_marker_fails_without_changing_cursor_state() {
    let temp = tempfile::tempdir().unwrap();
    let bulk_dir = temp.path().join(INDEX_DIR).join(BULK_PROJECTION_DIR);
    std::fs::create_dir_all(&bulk_dir).unwrap();
    let state_path = bulk_dir.join(BULK_PROJECTION_STATE_FILE);
    let original_state = br#"{"version":2,"next_author":1234,"segment_event_offset":0}"#;
    std::fs::write(&state_path, original_state).unwrap();
    std::fs::write(
        bulk_dir.join(REJECTED_SPOOL_EDGE_STATE_FILE),
        b"rejected fast-forward state",
    )
    .unwrap();

    let error = reject_spool_edge_state_marker(temp.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("rejected bulk projection fast-forward marker"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(state_path).unwrap(), original_state);
}

#[test]
fn duplicate_spool_apply_requires_the_same_payload_and_cid() {
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let original = event(&"01".repeat(32), 10, 1);
    let cid = Cid::public([1; 32]);
    spool.apply(vec![(original.clone(), cid.clone())]).unwrap();

    let duplicate = spool.apply(vec![(original.clone(), cid.clone())]).unwrap();
    assert_eq!(duplicate.skipped, 1);

    let mut mismatched = original.clone();
    mismatched.content = "different".to_string();
    assert!(spool
        .apply(vec![(mismatched, cid)])
        .unwrap_err()
        .to_string()
        .contains("payload differs from duplicate spool record"));
    assert!(spool
        .apply(vec![(original, Cid::public([2; 32]))])
        .unwrap_err()
        .to_string()
        .contains("CID differs from duplicate spool record"));
}

#[tokio::test]
async fn exact_spool_replay_reuses_cids_and_preserves_crash_replay_events() {
    let keys = Keys::generate();
    let profile = EventBuilder::new(Kind::Metadata, "profile")
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
    let note = EventBuilder::new(Kind::TextNote, "hello")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&keys)
        .unwrap();
    let original_events = [&profile, &note]
        .into_iter()
        .map(stored_event_from_nostr_sdk_event)
        .collect::<Vec<_>>();

    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store);
    let original_cids = event_store
        .store_event_blobs(original_events.clone())
        .await
        .unwrap();
    let mut event_cids = original_events
        .into_iter()
        .zip(original_cids)
        .collect::<Vec<_>>();
    event_cids.sort_unstable_by(|(left, _), (right, _)| right.id.cmp(&left.id));
    let (events, cids): (Vec<_>, Vec<_>) = event_cids.into_iter().unzip();
    assert!(events[0].id > events[1].id);
    spool
        .apply(events.clone().into_iter().zip(cids.clone()).collect())
        .unwrap();

    let replay = spool.plan_replay_batch(events.clone(), &cids).unwrap();
    assert_eq!(replay.reused_records, events.len());
    assert!(replay.missing_positions.is_empty());
    assert_eq!(
        replay
            .events
            .into_iter()
            .map(|(event, cid)| (event, cid.expect("reused target CID")))
            .collect::<Vec<_>>(),
        events.into_iter().zip(cids).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn exact_spool_replay_falls_back_for_missing_events_and_rejects_mismatch() {
    let keys = Keys::generate();
    let profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "profile")
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let note = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, "hello")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    );

    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store);
    let cids = event_store
        .store_event_blobs([profile.clone(), note.clone()])
        .await
        .unwrap();
    spool
        .apply(vec![(profile.clone(), cids[0].clone())])
        .unwrap();

    let partial = spool
        .plan_replay_batch(
            vec![profile.clone(), note.clone()],
            &[cids[0].clone(), cids[1].clone()],
        )
        .unwrap();
    assert_eq!(partial.reused_records, 1);
    assert_eq!(partial.missing_positions, vec![1]);
    assert_eq!(partial.events[1].0, note);
    assert_eq!(partial.events[0].1, Some(cids[0].clone()));
    assert_eq!(partial.events[1].1, None);

    let mut mismatched_payload = profile.clone();
    mismatched_payload.content = "different".to_string();
    let payload_error = spool
        .plan_replay_batch(vec![mismatched_payload], &[cids[0].clone()])
        .unwrap_err();
    assert!(payload_error
        .to_string()
        .contains("payload differs from durable spool record"));

    let cid_error = spool
        .plan_replay_batch(vec![profile], &[Cid::public([9; 32])])
        .unwrap_err();
    assert!(cid_error
        .to_string()
        .contains("CID differs from durable spool record"));
}

#[tokio::test]
async fn mixed_replay_reuses_only_fully_readable_matching_durable_candidates() {
    let keys = Keys::generate();
    let profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "profile")
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let note = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, "hello")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    );

    let durable = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(Arc::clone(&durable));
    let cids = event_store
        .store_event_blobs([profile.clone(), note.clone()])
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    spool
        .apply(vec![(profile.clone(), cids[0].clone())])
        .unwrap();

    let mut plan = spool
        .plan_replay_batch(
            vec![profile.clone(), note.clone()],
            &[cids[0].clone(), cids[1].clone()],
        )
        .unwrap();
    assert_eq!(
        plan.reuse_durable_candidates(&event_store, &cids)
            .await
            .unwrap(),
        1
    );
    assert!(plan.missing_positions.is_empty());
    assert_eq!(plan.events[1].1, Some(cids[1].clone()));
    let report = spool
        .apply(
            plan.events
                .into_iter()
                .map(|(event, cid)| (event, cid.unwrap()))
                .collect(),
        )
        .unwrap();
    assert_eq!(report.inserted, 1);
    assert_eq!(report.skipped, 1);

    let mut mismatched = note;
    mismatched.content = "different".to_string();
    let mismatch_temp = tempfile::tempdir().unwrap();
    let mismatch_spool = BulkProjectionSpool::open(mismatch_temp.path()).unwrap();
    let mut mismatch_plan = mismatch_spool
        .plan_replay_batch(vec![mismatched.clone()], &[cids[1].clone()])
        .unwrap();
    let mismatch_error = mismatch_plan
        .reuse_durable_candidates(&event_store, &[cids[1].clone()])
        .await
        .unwrap_err();
    assert!(mismatch_error
        .to_string()
        .contains("payload differs from durable target blob"));

    let missing_cid = Cid::public([9; 32]);
    let mut missing_plan = mismatch_spool
        .plan_replay_batch(vec![mismatched], std::slice::from_ref(&missing_cid))
        .unwrap();
    assert_eq!(
        missing_plan
            .reuse_durable_candidates(&event_store, &[missing_cid])
            .await
            .unwrap(),
        0
    );
    assert_eq!(missing_plan.missing_positions, vec![0]);
}

#[tokio::test]
async fn mixed_replay_stores_only_missing_events_and_preserves_replaceable_order() {
    let keys = Keys::generate();
    let old_profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "old")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let existing_note = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, "existing")
            .custom_created_at(Timestamp::from_secs(15))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let new_profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "new")
            .custom_created_at(Timestamp::from_secs(30))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let losing_profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "loser")
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let replay_events = vec![
        new_profile.clone(),
        old_profile.clone(),
        existing_note.clone(),
        losing_profile.clone(),
    ];

    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(Arc::clone(&store));
    let initial_events = vec![old_profile, existing_note.clone()];
    let initial_cids = event_store
        .store_event_blobs(initial_events.clone())
        .await
        .unwrap();
    let replay_cids = event_store
        .store_event_blobs(replay_events.clone())
        .await
        .unwrap();

    let baseline_dir = tempfile::tempdir().unwrap();
    let baseline = BulkProjectionSpool::open(baseline_dir.path()).unwrap();
    baseline
        .apply(
            initial_events
                .clone()
                .into_iter()
                .zip(initial_cids.clone())
                .collect(),
        )
        .unwrap();
    let baseline_report = baseline
        .apply(
            replay_events
                .clone()
                .into_iter()
                .zip(replay_cids.clone())
                .collect(),
        )
        .unwrap();

    let optimized_dir = tempfile::tempdir().unwrap();
    let optimized = BulkProjectionSpool::open(optimized_dir.path()).unwrap();
    optimized
        .apply(initial_events.into_iter().zip(initial_cids).collect())
        .unwrap();
    let plan = optimized
        .plan_replay_batch(replay_events, &replay_cids)
        .unwrap();
    assert_eq!(plan.reused_records, 2);
    assert_eq!(plan.missing_positions, vec![0, 3]);
    assert_eq!(plan.events[0].0, new_profile);
    assert_eq!(plan.events[3].0, losing_profile);
    let missing_cids = event_store
        .store_event_blobs(
            plan.events
                .iter()
                .filter(|(_, cid)| cid.is_none())
                .map(|(event, _)| event.clone()),
        )
        .await
        .unwrap();
    let mut missing_cids = missing_cids.into_iter();
    let planned_events = plan
        .events
        .into_iter()
        .map(|(event, existing_cid)| {
            (
                event,
                existing_cid.unwrap_or_else(|| missing_cids.next().unwrap()),
            )
        })
        .collect();
    assert!(missing_cids.next().is_none());
    let optimized_report = optimized.apply(planned_events).unwrap();

    assert_eq!(optimized_report.inserted, baseline_report.inserted);
    assert_eq!(optimized_report.replaced, baseline_report.replaced);
    assert_eq!(optimized_report.skipped, baseline_report.skipped);
    assert_eq!(
        optimized_report.retained_events,
        baseline_report.retained_events
    );
    for index in NostrEventIndex::ALL {
        assert_eq!(
            optimized
                .build_index_root(index, Arc::clone(&store), 8)
                .await
                .unwrap(),
            baseline
                .build_index_root(index, Arc::clone(&store), 8)
                .await
                .unwrap()
        );
    }
    assert_eq!(
        optimized_report.retained_events,
        vec![new_profile, existing_note]
    );
}

#[tokio::test]
#[ignore = "requires an explicit real staged store and isolated LMDB target"]
async fn benchmark_real_bulk_projection_phase() {
    use hashtree_cli::HashtreeStore;
    use hashtree_config::StorageBackend;
    use hashtree_lmdb::{PoolMemberConfig, PoolStore, PoolStoreConfig};

    let stage_dir = PathBuf::from(
        std::env::var("HTREE_BULK_BENCH_STAGE_DIR")
            .expect("HTREE_BULK_BENCH_STAGE_DIR must name a copied or read-only real stage"),
    );
    let output_dir = PathBuf::from(
        std::env::var("HTREE_BULK_BENCH_OUTPUT_DIR")
            .expect("HTREE_BULK_BENCH_OUTPUT_DIR must name an isolated benchmark target"),
    );
    let author = std::env::var("HTREE_BULK_BENCH_AUTHOR")
        .unwrap_or_else(|_| "3078".to_string())
        .parse::<usize>()
        .expect("HTREE_BULK_BENCH_AUTHOR");
    let offset = std::env::var("HTREE_BULK_BENCH_OFFSET")
        .unwrap_or_else(|_| "0".to_string())
        .parse::<usize>()
        .expect("HTREE_BULK_BENCH_OFFSET");
    let limit = std::env::var("HTREE_BULK_BENCH_LIMIT")
        .unwrap_or_else(|_| "32768".to_string())
        .parse::<usize>()
        .expect("HTREE_BULK_BENCH_LIMIT");
    let mode =
        std::env::var("HTREE_BULK_BENCH_MODE").expect("HTREE_BULK_BENCH_MODE must be explicit");
    let target_map_size_bytes = std::env::var("HTREE_BULK_BENCH_TARGET_MAP_SIZE_BYTES")
        .unwrap_or_else(|_| (1024_u64 * 1024 * 1024).to_string())
        .parse::<u64>()
        .expect("HTREE_BULK_BENCH_TARGET_MAP_SIZE_BYTES");
    let target_capacity_bytes = std::env::var("HTREE_BULK_BENCH_TARGET_CAPACITY_BYTES")
        .unwrap_or_else(|_| (16_u64 * 1024 * 1024 * 1024).to_string())
        .parse::<u64>()
        .expect("HTREE_BULK_BENCH_TARGET_CAPACITY_BYTES");

    let stage =
        HashtreeStore::with_options_and_backend(&stage_dir, None, 0, false, &StorageBackend::Lmdb)
            .expect("open real stage");
    let segment = load_stage_segment(&stage_dir, author).expect("load real staged segment");
    let end = offset.saturating_add(limit).min(segment.event_cids.len());
    let cids = segment.event_cids[offset..end]
        .iter()
        .map(|cid| parse_root_text(cid))
        .collect::<Result<Vec<_>>>()
        .expect("parse real staged CIDs");
    let stage_event_store = NostrEventStore::new(stage.store_arc());
    let load_started = Instant::now();
    let blobs = stage_event_store
        .load_validated_event_blobs(cids)
        .await
        .expect("load real validated event blobs");
    let load_ms = load_started.elapsed().as_millis();

    match mode.as_str() {
        "target-copy" | "target-copy-mixed" | "target-legacy" => {
            let open_target = || {
                let target = PoolStore::open(output_dir.join("pool"), PoolStoreConfig::default())
                    .expect("open isolated PoolStore target");
                if target
                    .members()
                    .expect("list isolated target members")
                    .is_empty()
                {
                    let mut member =
                        PoolMemberConfig::new(output_dir.join("member"), target_capacity_bytes)
                            .with_map_size_bytes(target_map_size_bytes)
                            .with_external_blobs(
                                output_dir.join("external"),
                                1,
                                true,
                                Some(64 * 1024 * 1024),
                            );
                    member.max_read_concurrency = 64;
                    member.max_write_concurrency = 8;
                    target
                        .add_member(member)
                        .expect("add isolated PoolStore target member");
                }
                Arc::new(target)
            };
            let mut target = open_target();
            if mode == "target-copy-mixed" {
                let event_store = NostrEventStore::new(Arc::clone(&target));
                event_store
                    .store_validated_event_blobs(blobs.iter().step_by(2))
                    .await
                    .expect("pre-store alternating real event blobs");
                target.force_sync().expect("sync mixed target prefix");
                drop(event_store);
                drop(target);
                target = open_target();
            }
            let target_event_store = NostrEventStore::new(Arc::clone(&target));
            let write_started = Instant::now();
            let target_cids = if mode.starts_with("target-copy") {
                target_event_store
                    .store_validated_event_blobs(&blobs)
                    .await
                    .expect("copy real event blobs")
            } else {
                target_event_store
                    .store_event_blobs(blobs.iter().map(|blob| blob.event().clone()))
                    .await
                    .expect("re-encode real event blobs")
            };
            let write_ms = write_started.elapsed().as_millis();
            let sync_started = Instant::now();
            target.force_sync().expect("sync isolated target");
            let sync_ms = sync_started.elapsed().as_millis();
            assert_eq!(
                target_cids,
                blobs
                    .iter()
                    .map(|blob| blob.cid().clone())
                    .collect::<Vec<_>>()
            );
            drop(target_event_store);
            drop(target);

            let reopen_started = Instant::now();
            let reopened = open_target();
            let reopened_event_store = NostrEventStore::new(Arc::clone(&reopened));
            let loaded = reopened_event_store
                .load_event_blobs(target_cids)
                .await
                .expect("read back copied events after target reopen");
            assert_eq!(
                loaded,
                blobs
                    .iter()
                    .map(|blob| blob.event().clone())
                    .collect::<Vec<_>>()
            );
            let reopen_readback_ms = reopen_started.elapsed().as_millis();
            eprintln!(
                "real_bulk_bench mode={mode} author={author} offset={offset} events={} \
                 live_bytes={} load_ms={load_ms} write_ms={write_ms} sync_ms={sync_ms} \
                 reopen_readback_ms={reopen_readback_ms} \
                 target_map_size_bytes={target_map_size_bytes} \
                 target_capacity_bytes={target_capacity_bytes}",
                blobs.len(),
                segment.live_bytes_selected,
            );
        }
        "spool" => {
            let spool = BulkProjectionSpool::open(&output_dir).expect("open isolated spool");
            let events = blobs
                .iter()
                .map(|blob| (blob.event().clone(), blob.cid().clone()))
                .collect::<Vec<_>>();
            let apply = spool
                .apply(events)
                .expect("apply real events to isolated spool");
            eprintln!(
                "real_bulk_bench mode={mode} author={author} offset={offset} events={} \
                 live_bytes={} load_ms={load_ms} inserted={} replaced={} skipped={} \
                 index_entries={} spool_write_ms={} spool_sync_ms={}",
                blobs.len(),
                segment.live_bytes_selected,
                apply.inserted,
                apply.replaced,
                apply.skipped,
                apply.index_entries,
                apply.spool_write_ms,
                apply.spool_sync_ms,
            );
        }
        other => panic!("unsupported HTREE_BULK_BENCH_MODE {other}"),
    }
}

#[test]
fn entry_trie_streams_long_keys_in_logical_order_across_pages() {
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let mut expected = BTreeMap::new();
    for key in [
        String::new(),
        "a".repeat(399),
        "a".repeat(400),
        format!("{}a", "a".repeat(400)),
        format!("{}b", "a".repeat(400)),
        format!("{}é", "a".repeat(399)),
        "z".repeat(1_024),
    ]
    .into_iter()
    .chain((0..4_100).map(|number| format!("page-{number:04}")))
    {
        expected.insert(key.clone(), Cid::public(sha256(key.as_bytes())));
    }
    let mut wtxn = spool.env.write_txn().unwrap();
    for (key, cid) in &expected {
        spool
            .put_entry(&mut wtxn, NostrEventIndex::ByTag, key, cid)
            .unwrap();
    }
    wtxn.commit().unwrap();

    let mut cursor = EntryTrieCursor::new(&spool, NostrEventIndex::ByTag);
    let mut actual = Vec::new();
    while let Some(entry) = cursor.next_entry().unwrap() {
        actual.push(entry);
    }
    assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
}

#[tokio::test]
async fn bulk_projection_root_matches_the_incremental_collection_writer() {
    let keys = Keys::generate();
    let long_tag = "x".repeat(1_024);
    let old = EventBuilder::new(Kind::Metadata, "old profile")
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
    let note = EventBuilder::new(Kind::TextNote, "hello")
        .tags([
            Tag::parse(["t", "Hashtree"]).unwrap(),
            Tag::parse(["r", long_tag.as_str()]).unwrap(),
        ])
        .custom_created_at(Timestamp::from_secs(15))
        .sign_with_keys(&keys)
        .unwrap();
    let new = EventBuilder::new(Kind::Metadata, "new profile")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&keys)
        .unwrap();
    let events = [&old, &note, &new]
        .into_iter()
        .map(stored_event_from_nostr_sdk_event)
        .collect::<Vec<_>>();

    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::with_options(
        Arc::clone(&store),
        NostrEventStoreOptions {
            btree_order: Some(8),
            ..NostrEventStoreOptions::default()
        },
    );
    let cids = event_store.store_event_blobs(events.clone()).await.unwrap();
    spool
        .apply(events.clone().into_iter().zip(cids).collect())
        .unwrap();

    let mut roots = BTreeMap::new();
    for index in NostrEventIndex::ALL {
        roots.insert(
            index,
            spool
                .build_index_root(index, Arc::clone(&store), 8)
                .await
                .unwrap(),
        );
    }
    let bulk = event_store
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    let mut incremental = None;
    for event in events {
        incremental = Some(event_store.add(incremental.as_ref(), event).await.unwrap());
    }

    assert_eq!(Some(bulk), incremental);
}
