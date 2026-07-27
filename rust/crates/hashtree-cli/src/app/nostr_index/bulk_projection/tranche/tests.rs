use super::super::super::{
    reset_stage_segment_io_counts, stage_segment_io_counts, STAGE_FORMAT_VERSION,
};
use super::*;

use hashtree_config::StorageBackend;
use hashtree_nostr::stored_event_from_nostr_sdk_event;
use nostr::{EventBuilder, Keys, Kind, Timestamp};
use tempfile::TempDir;

fn test_rank_authority(
    directory: &Path,
    pubkey: &str,
    eligible_authors_sha256: &str,
) -> TrustedProfileRankDecisions {
    const FORMAT: &str = "iris-social/profile-search-v3-rank-decisions@1";
    let row = serde_json::to_string(&serde_json::json!([pubkey, "eligible", 0]))
        .expect("encode semantic rank row");
    let mut semantic = Sha256::new();
    semantic.update(FORMAT.as_bytes());
    semantic.update(b"\n");
    semantic.update(row.as_bytes());
    semantic.update(b"\n");
    let semantic_sha256 = hex::encode(semantic.finalize());
    let header = format!(
        r#"{{"format":"{FORMAT}","eligibleRanksSha256":"{semantic_sha256}","recordCount":1}}"#
    );
    let record = format!(r#"{{"pubkey":"{pubkey}","decision":"eligible","rankHint":0}}"#);
    let decisions_bytes = format!("{header}\n{record}\n").into_bytes();
    let decisions_sha256 = bytes_sha256(&decisions_bytes);
    let report = serde_json::json!({
        "format": "iris-social/profile-search-v3-rank-decision-artifacts@1",
        "censusFormat": "iris-social/social-graph-crawl-census@2",
        "socialGraphRoot": "a".repeat(64),
        "socialGraphSha256": "b".repeat(64),
        "eligibleAuthorsSha256": eligible_authors_sha256,
        "overmuteThreshold": 1,
        "maxDistance": 4,
        "rankPolicy": "follow-distance@1",
        "exclusionPolicy": "all-nonselected-graph-identities@1",
        "recordCount": 1,
        "eligibleCount": 1,
        "excludedCount": 0,
        "reachableCount": 1,
        "reachableOvermutedCount": 0,
        "distanceExcludedCount": 0,
        "unreachableCount": 0,
        "allGraphOvermutedCount": 0,
        "rankDecisionsSha256": semantic_sha256,
        "rankDecisionsFileSha256": decisions_sha256,
    });
    let report_bytes = format!(
        "{}\n",
        serde_json::to_string_pretty(&report).expect("encode rank report")
    )
    .into_bytes();
    let report_sha256 = bytes_sha256(&report_bytes);
    let decisions_path = directory.join("rank-decisions.jsonl");
    let report_path = directory.join("rank-report.json");
    std::fs::write(&decisions_path, decisions_bytes).expect("write generated rank decisions");
    std::fs::write(&report_path, report_bytes).expect("write generated rank report");
    load_pinned_profile_rank_decisions(
        &decisions_path,
        &decisions_sha256,
        &report_path,
        &report_sha256,
    )
    .expect("load generated strict rank authority")
}

fn staged_segment(
    start_author: usize,
    end_author: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
    event_cids: Vec<String>,
) -> StagedAuthorSegment {
    StagedAuthorSegment {
        version: STAGE_FORMAT_VERSION,
        start_author,
        end_author,
        events_seen,
        events_selected,
        live_bytes_selected,
        event_cids,
    }
}

fn prefix_target(
    boundary: usize,
    durable_next_author: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
) -> StagePrefixTarget {
    StagePrefixTarget {
        boundary,
        durable_next_author,
        events_seen,
        events_selected,
        live_bytes_selected,
    }
}

fn stage_policy(author_count: usize, segment_width: usize) -> IndexedNostrCrawlPolicy {
    IndexedNostrCrawlPolicy {
        base_root: None,
        author_allowlist_sha256: "aa".repeat(32),
        author_count,
        relays: vec!["wss://relay.example".to_string()],
        require_all_relays: false,
        max_events_seen: None,
        max_authors: author_count,
        max_follow_distance: Some(0),
        max_live_bytes: u64::MAX,
        author_batch_size: segment_width,
        checkpoint_authors: segment_width,
        per_author_event_limit: 256,
        per_author_kind_event_limit: None,
        per_author_live_bytes: None,
        fetch_timeout_millis: 30_000,
        relay_event_max_bytes: None,
        global_relay_scan: false,
        full_author_history: true,
        negentropy_only: false,
        relay_page_size: 1_000,
        max_relay_pages: 67,
        kinds: None,
    }
}

#[test]
fn phase_schema_has_explicit_non_promoting_boundaries() {
    let phases = [
        (TranchePhase::Prepare, "\"prepare\""),
        (TranchePhase::Appending, "\"appending\""),
        (TranchePhase::Freeze, "\"freeze\""),
        (TranchePhase::Building, "\"building\""),
        (TranchePhase::Candidate, "\"candidate\""),
        (TranchePhase::Verified, "\"verified\""),
        (TranchePhase::Publishing, "\"publishing\""),
        (TranchePhase::Promoted, "\"promoted\""),
    ];
    for (phase, encoded) in phases {
        assert_eq!(serde_json::to_string(&phase).unwrap(), encoded);
    }
}

#[test]
fn prefix_attestation_pins_raw_segments_and_cids_not_mutable_stage_totals() {
    let tmp = TempDir::new().expect("tempdir");
    let policy = stage_policy(4, 2);
    let first = staged_segment(
        0,
        2,
        7,
        2,
        512,
        vec![cid_to_nhash(&hashtree_core::Cid {
            hash: [0; 32],
            key: None,
        })
        .expect("encode test event CID")],
    );
    super::super::super::persist_stage_segment(tmp.path(), &first, &policy)
        .expect("publish first real staged segment");
    let initial = attest_stage_prefix(
        tmp.path(),
        prefix_target(2, 2, 7, 2, 512),
        "11".repeat(32),
        &policy,
    )
    .expect("attest first prefix");
    assert_eq!(initial.segment_count, 1);
    assert_eq!(initial.event_cid_count, 1);

    let second = staged_segment(2, 4, 3, 0, 0, Vec::new());
    super::super::super::persist_stage_segment(tmp.path(), &second, &policy)
        .expect("publish later real staged segment");
    let repeated = attest_stage_prefix(
        tmp.path(),
        prefix_target(2, 4, 7, 2, 512),
        "22".repeat(32),
        &policy,
    )
    .expect("reattest immutable prefix after staging advances");
    assert!(initial.immutable_prefix_eq(&repeated));
    assert_ne!(
        initial.observed_stage_state_sha256,
        repeated.observed_stage_state_sha256
    );

    let extended = attest_stage_prefix(
        tmp.path(),
        prefix_target(4, 4, 10, 2, 512),
        "33".repeat(32),
        &policy,
    )
    .expect("attest extended contiguous prefix");
    assert_eq!(extended.segment_count, 2);
    assert_eq!(extended.event_cid_count, 1);
    assert_ne!(extended.segment_chain_sha256, initial.segment_chain_sha256);

    let (path, bytes, loaded) =
        super::super::super::load_stage_segment_with_bytes(tmp.path(), 2, &policy)
            .expect("target second staged segment");
    let mut rolling = initial;
    extend_stage_prefix(&mut rolling, &path, &bytes, &loaded, &"33".repeat(32))
        .expect("extend rolling prefix");
    assert!(rolling.immutable_prefix_eq(&extended));
}

#[test]
fn prefix_attestation_detects_post_publish_segment_mutation() {
    let tmp = TempDir::new().expect("tempdir");
    let policy = stage_policy(1, 1);
    let segment = staged_segment(0, 1, 1, 0, 0, Vec::new());
    super::super::super::persist_stage_segment(tmp.path(), &segment, &policy)
        .expect("publish real staged segment");
    let sealed = attest_stage_prefix(
        tmp.path(),
        prefix_target(1, 1, 1, 0, 0),
        "44".repeat(32),
        &policy,
    )
    .expect("seal prefix");

    let mut mutated = segment;
    mutated.events_seen = 2;
    let path = super::super::super::stage_segment_path(tmp.path(), 0, 1);
    let mut bytes = serde_json::to_vec(&mutated).expect("encode mutation");
    bytes.push(b'\n');
    std::fs::write(&path, bytes).expect("simulate post-publish disk mutation");

    let observed = attest_stage_prefix(
        tmp.path(),
        prefix_target(1, 1, 2, 0, 0),
        "55".repeat(32),
        &policy,
    )
    .expect_err("terminal scan must reject a body changed after its claim");
    assert!(observed
        .to_string()
        .contains("differs from its immutable per-start claim"));
    assert_eq!(sealed.next_author, 1);
}

#[test]
fn terminal_scan_accepts_claimed_mixed_width_history() {
    let tmp = TempDir::new().expect("tempdir");
    let narrow = stage_policy(5, 1);
    let first = staged_segment(0, 1, 1, 0, 0, Vec::new());
    super::super::super::persist_stage_segment(tmp.path(), &first, &narrow)
        .expect("publish narrow historical segment");
    let wide = stage_policy(5, 2);
    let second = staged_segment(1, 3, 2, 0, 0, Vec::new());
    super::super::super::persist_stage_segment(tmp.path(), &second, &wide)
        .expect("publish wider later segment");

    let sealed = attest_stage_prefix(
        tmp.path(),
        prefix_target(3, 3, 3, 0, 0),
        "56".repeat(32),
        &wide,
    )
    .expect("mixed-width claimed history is unambiguous");
    assert_eq!(sealed.segment_count, 2);
    assert_eq!(sealed.next_author, 3);
}

#[test]
fn terminal_scan_rejects_unclaimed_body() {
    let tmp = TempDir::new().expect("tempdir");
    let policy = stage_policy(1, 1);
    let segment = staged_segment(0, 1, 0, 0, 0, Vec::new());
    let path = super::super::super::stage_segment_path(tmp.path(), 0, 1);
    let mut bytes = serde_json::to_vec(&segment).expect("encode unclaimed body");
    bytes.push(b'\n');
    super::super::super::persist_immutable_bytes(&path, &bytes, "unclaimed test body")
        .expect("publish unclaimed body");

    let error = stage_segment_catalog(tmp.path(), &policy, 1)
        .expect_err("terminal scan must reject a body without a claim");
    assert!(error
        .to_string()
        .contains("bodies and per-start boundary claims differ"));
}

#[test]
fn terminal_scan_rejects_overlapping_mixed_width_claims() {
    let tmp = TempDir::new().expect("tempdir");
    let wide = stage_policy(3, 3);
    super::super::super::persist_stage_segment(
        tmp.path(),
        &staged_segment(0, 3, 0, 0, 0, Vec::new()),
        &wide,
    )
    .expect("publish wide claimed segment");
    let narrow = stage_policy(3, 1);
    super::super::super::persist_stage_segment(
        tmp.path(),
        &staged_segment(1, 2, 0, 0, 0, Vec::new()),
        &narrow,
    )
    .expect("publish overlapping claimed segment");

    let error = stage_segment_catalog(tmp.path(), &wide, 3)
        .expect_err("full scan must reject an overlapping interior start");
    assert!(error
        .to_string()
        .contains("not one contiguous non-overlapping chain"));
}

#[test]
fn terminal_scan_rejects_body_at_durable_cursor() {
    let tmp = TempDir::new().expect("tempdir");
    let policy = stage_policy(2, 1);
    for segment in [
        staged_segment(0, 1, 0, 0, 0, Vec::new()),
        staged_segment(1, 2, 0, 0, 0, Vec::new()),
    ] {
        super::super::super::persist_stage_segment(tmp.path(), &segment, &policy)
            .expect("publish claimed segment");
    }

    let error = stage_segment_catalog(tmp.path(), &policy, 1)
        .expect_err("body beginning at durable cursor is not checkpointed");
    assert!(error.to_string().contains("beyond durable cursor"));
}

#[test]
fn terminal_prefix_scan_rejects_duplicate_segment_starts() {
    let tmp = TempDir::new().expect("tempdir");
    let policy = stage_policy(2, 1);
    let canonical = staged_segment(0, 1, 0, 0, 0, Vec::new());
    super::super::super::persist_stage_segment(tmp.path(), &canonical, &policy)
        .expect("publish canonical segment");
    let duplicate = staged_segment(0, 2, 0, 0, 0, Vec::new());
    let duplicate_path = super::super::super::stage_segment_path(tmp.path(), 0, 2);
    let mut duplicate_bytes = serde_json::to_vec(&duplicate).expect("encode duplicate segment");
    duplicate_bytes.push(b'\n');
    std::fs::write(&duplicate_path, duplicate_bytes).expect("write duplicate catalog entry");

    let error = attest_stage_prefix(
        tmp.path(),
        prefix_target(1, 1, 0, 0, 0),
        "66".repeat(32),
        &policy,
    )
    .expect_err("terminal full scan must reject duplicate starts");
    assert!(error.to_string().contains("duplicate staged segment start"));
}

#[tokio::test]
async fn append_reads_one_segment_body_across_many_event_checkpoints() {
    const HISTORICAL_CLUTTER_FILES: usize = 2_048;

    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().join("projection");
    let staging_data_dir = tmp.path().join("staging");
    let projection_store = hashtree_cli::HashtreeStore::with_options_and_backend(
        &data_dir,
        None,
        0,
        false,
        &StorageBackend::Lmdb,
    )
    .expect("open real projection LMDB");
    let staging_store = hashtree_cli::HashtreeStore::with_options_and_backend(
        &staging_data_dir,
        None,
        0,
        false,
        &StorageBackend::Lmdb,
    )
    .expect("open real staging LMDB");
    let graph = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
        &data_dir,
        projection_store.store_arc(),
        Some(128 * 1024 * 1024),
    )
    .expect("open real profile index LMDB");

    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();
    let events = (0..3)
        .map(|index| {
            EventBuilder::new(Kind::TextNote, format!("append body read {index}"))
                .custom_created_at(Timestamp::from_secs(100 + index))
                .sign_with_keys(&keys)
                .expect("sign generated staged event")
        })
        .collect::<Vec<_>>();
    let stored = events
        .iter()
        .map(stored_event_from_nostr_sdk_event)
        .collect::<Vec<_>>();
    let event_store_options = NostrEventStoreOptions {
        btree_order: Some(8),
        btree_update_concurrency: Some(1),
        index_commit_batch_size: Some(1),
    };
    let staging_events =
        NostrEventStore::with_options(staging_store.store_arc(), event_store_options);
    let event_cids = staging_events
        .store_event_blobs(stored)
        .await
        .expect("store real staged event blobs");
    staging_store.force_sync().expect("sync staged event blobs");

    let mut policy = stage_policy(1, 1);
    let allowlist_bytes = format!("{pubkey}\n").into_bytes();
    policy.author_allowlist_sha256 = bytes_sha256(&allowlist_bytes);
    let segment = staged_segment(
        0,
        1,
        events.len(),
        events.len(),
        0,
        event_cids
            .iter()
            .map(cid_to_nhash)
            .collect::<Result<Vec<_>>>()
            .expect("encode staged event CIDs"),
    );
    super::super::super::persist_stage_segment(&staging_data_dir, &segment, &policy)
        .expect("publish one real claimed staged segment");
    let historical_clutter_paths = (0..HISTORICAL_CLUTTER_FILES)
        .map(|offset| {
            let start = 10_000 + offset;
            let path = super::super::super::stage_segment_path(&staging_data_dir, start, start + 1);
            std::fs::write(&path, b"unreadable historical segment body\n")
                .expect("write generated historical segment clutter");
            path
        })
        .collect::<Vec<_>>();
    let stage_state = StagedNostrCrawlState {
        version: STAGE_FORMAT_VERSION,
        author_allowlist_source: None,
        policy: policy.clone(),
        next_author: 1,
        events_seen: events.len(),
        events_selected: events.len(),
        live_bytes_selected: 0,
    };
    super::super::super::persist_stage_state(&staging_data_dir, &stage_state)
        .expect("persist real staging watermark");
    let stage_state_bytes = std::fs::read(staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE))
        .expect("read stage state");
    let stage_state_sha256 = bytes_sha256(&stage_state_bytes);

    let rank_source = tmp.path().join("rank-source");
    std::fs::create_dir_all(&rank_source).expect("create generated rank source directory");
    let rank_authority =
        test_rank_authority(&rank_source, &pubkey, &policy.author_allowlist_sha256);
    let (_, spool_path) = bulk_paths(&data_dir);
    let initial_spool = BulkProjectionSpool::open(&spool_path).expect("create real spool LMDB");
    drop(initial_spool);
    let (spool_identity, marker_bytes) =
        spool_identity(&data_dir).expect("capture real spool identity");
    let (state_path, seals_dir, evidence_dir, _, marker_path) = tranche_paths(&data_dir);
    persist_immutable_bytes(&marker_path, &marker_bytes, "test spool identity")
        .expect("persist spool marker");
    persist_immutable_bytes(
        &hashtree_cli::socialgraph::profile_publication_fence_path(&data_dir),
        PROFILE_PUBLICATION_FENCE_BYTES,
        "test profile publication fence",
    )
    .expect("persist profile publication fence");
    let (copied_decisions, copied_report) =
        profile_rank_evidence_paths(&evidence_dir, &rank_authority.evidence);
    persist_immutable_bytes(
        &copied_decisions,
        &rank_authority.decisions_bytes,
        "test copied rank decisions",
    )
    .expect("persist copied rank decisions");
    persist_immutable_bytes(
        &copied_report,
        &rank_authority.report_bytes,
        "test copied rank report",
    )
    .expect("persist copied rank report");
    persist_immutable_bytes(
        &ordered_allowlist_evidence_path(&evidence_dir, &policy.author_allowlist_sha256),
        &allowlist_bytes,
        "test canonical allowlist",
    )
    .expect("persist canonical allowlist");

    let mut built_roots = BTreeMap::new();
    for (position, index) in NostrEventIndex::ALL.into_iter().enumerate() {
        built_roots.insert(
            index.stable_id(),
            cid_to_nhash(&hashtree_core::Cid::public([position as u8 + 1; 32]))
                .expect("encode generated index root"),
        );
    }
    let candidate = CandidatePin {
        root: cid_to_nhash(&hashtree_core::Cid::public([240; 32])).expect("encode candidate root"),
        built_roots,
    };
    let evidence = AuditEvidencePin {
        sha256: "1".repeat(64),
        candidate_root: candidate.root.clone(),
        v2_state_sha256: "2".repeat(64),
        stage_state_sha256: stage_state_sha256.clone(),
        trusted_policy_sha256: "3".repeat(64),
        trusted_full_author_count: 1,
        pool_catalog_sha256: "4".repeat(64),
        pool_manifest_sha256: "5".repeat(64),
        profile_by_pubkey_root_file_sha256: "6".repeat(64),
        profile_search_root_file_sha256: "7".repeat(64),
        profile_follow_distance_seal_sha256: "8".repeat(64),
        profile_distance_provenance: rank_authority.evidence.clone(),
    };
    let serving = ServingRootPin {
        root: candidate.root.clone(),
        event_id: "9".repeat(64),
        event_sha256: "a".repeat(64),
        event_pubkey: "b".repeat(64),
        event_created_at: 1,
        tree_name: "social.iris.to".to_string(),
    };
    let prefix = StagePrefixSeal::empty(stage_state_sha256);
    let seal = TrancheSeal {
        version: TRANCHE_STATE_VERSION,
        generation: 0,
        parent_seal_sha256: None,
        purpose: TrancheSealPurpose::Prepare,
        policy: policy.clone(),
        ordered_allowlist_sha256: policy.author_allowlist_sha256.clone(),
        ordered_allowlist_count: 1,
        prefix: prefix.clone(),
        spool_identity: spool_identity.clone(),
        internal_candidate: Some(candidate.clone()),
        evidence: Some(evidence.clone()),
        profile_rank_authority: rank_authority.evidence.clone(),
        frozen_profile_distances: None,
        serving: serving.clone(),
        publication_intent: None,
        publication_receipt: None,
    };
    let active_seal_sha256 = persist_seal(&seals_dir, &seal).expect("persist active Prepare seal");
    let state = BulkTrancheState {
        version: TRANCHE_STATE_VERSION,
        phase: TranchePhase::Appending,
        generation: 0,
        policy,
        ordered_allowlist_sha256: bytes_sha256(&allowlist_bytes),
        ordered_allowlist_count: 1,
        active_seal_sha256: Some(active_seal_sha256),
        pending_seal_sha256: None,
        serving,
        last_validated: candidate,
        last_evidence: evidence,
        profile_rank_authority: rank_authority.evidence,
        spool_identity,
        btree_order: 8,
        btree_update_concurrency: 1,
        index_commit_batch_size: 1,
        working: WorkingProjection {
            next_author: 0,
            segment_event_offset: 0,
            active_segment_sha256: None,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
            rolling_prefix: prefix,
            frozen_profile_distances: None,
            built_roots: BTreeMap::new(),
            candidate_root: None,
            frozen_prefix: None,
        },
        publication_intent: None,
        publication_receipt: None,
    };
    let state_sha256 = persist_state(&state_path, &state).expect("persist Appending state");
    let stores = ProjectionStores {
        durable: &projection_store,
        staging: &staging_store,
        graph: &graph,
    };

    reset_stage_segment_io_counts();
    let output = append_bulk_tranche(
        stores,
        &data_dir,
        BulkTrancheAppendOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: state_sha256,
            max_segments: 1,
            out: None,
        },
    )
    .await
    .expect("append one segment through three production replay checkpoints");
    assert_eq!(output.next_author, 1);
    assert_eq!(
        stage_segment_io_counts(),
        (0, 1),
        "production append must ignore 2,048 historical bodies and read the active body once"
    );

    for path in historical_clutter_paths {
        std::fs::remove_file(path).expect("remove generated historical segment clutter");
    }
    let freeze_output = freeze_bulk_tranche(
        &data_dir,
        BulkTrancheFreezeOptions {
            staging_data_dir,
            expected_state_sha256: output.state_sha256,
            through_author: 1,
            out: None,
        },
    )
    .expect("freeze the fully appended generated prefix");
    let (frozen_state, _, frozen_state_sha256) = load_state(&state_path)
        .expect("load frozen state")
        .expect("frozen state exists");
    assert_eq!(frozen_state_sha256, freeze_output.state_sha256);
    assert_eq!(frozen_state.phase, TranchePhase::Freeze);
    let frozen_seal = load_seal(
        &seals_dir,
        frozen_state.generation,
        frozen_state
            .active_seal_sha256
            .as_deref()
            .expect("frozen active seal"),
    )
    .expect("load frozen seal");
    validate_active_seal(&frozen_state, &frozen_seal)
        .expect("a Freeze seal must validate against the state it created");
}
