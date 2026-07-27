use super::super::super::{
    reset_stage_segment_io_counts, stage_segment_io_counts, STAGE_FORMAT_VERSION,
};
use super::*;

use hashtree_config::StorageBackend;
use hashtree_nostr::{stored_event_from_nostr_sdk_event, HASHTREE_ROOT_KIND};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use tempfile::TempDir;

fn test_rank_authority(
    directory: &Path,
    pubkey: &str,
    eligible_authors_sha256: &str,
) -> TrustedProfileRankDecisions {
    test_rank_authority_for_pubkeys(directory, &[pubkey.to_string()], eligible_authors_sha256)
}

fn test_rank_authority_for_pubkeys(
    directory: &Path,
    pubkeys: &[String],
    eligible_authors_sha256: &str,
) -> TrustedProfileRankDecisions {
    const FORMAT: &str = "iris-social/profile-search-v3-rank-decisions@1";
    let mut pubkeys = pubkeys.to_vec();
    pubkeys.sort();
    pubkeys.dedup();
    assert!(!pubkeys.is_empty(), "rank authority needs eligible authors");
    let mut semantic = Sha256::new();
    semantic.update(FORMAT.as_bytes());
    semantic.update(b"\n");
    let mut records = Vec::with_capacity(pubkeys.len());
    for pubkey in &pubkeys {
        let row = serde_json::to_string(&serde_json::json!([pubkey, "eligible", 0]))
            .expect("encode semantic rank row");
        semantic.update(row.as_bytes());
        semantic.update(b"\n");
        records.push(format!(
            r#"{{"pubkey":"{pubkey}","decision":"eligible","rankHint":0}}"#
        ));
    }
    let semantic_sha256 = hex::encode(semantic.finalize());
    let header = format!(
        r#"{{"format":"{FORMAT}","eligibleRanksSha256":"{semantic_sha256}","recordCount":{}}}"#,
        pubkeys.len()
    );
    let decisions_bytes = format!("{header}\n{}\n", records.join("\n")).into_bytes();
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
        "recordCount": pubkeys.len(),
        "eligibleCount": pubkeys.len(),
        "excludedCount": 0,
        "reachableCount": pubkeys.len(),
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

#[test]
fn prepare_transition_uses_canonical_policy_across_ephemeral_allowlist_ports() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().join("projection");
    let staging_data_dir = tmp.path().join("staging");
    let source_dir = tmp.path().join("sources");
    std::fs::create_dir_all(&source_dir).expect("create generated source directory");

    let mut pubkeys = vec![
        Keys::generate().public_key().to_hex(),
        Keys::generate().public_key().to_hex(),
    ];
    pubkeys.sort();
    let allowlist_bytes = format!("{}\n", pubkeys.join("\n")).into_bytes();
    let allowlist_path = source_dir.join("eligible-authors.txt");
    std::fs::write(&allowlist_path, &allowlist_bytes).expect("write generated canonical allowlist");
    let mut policy = stage_policy(pubkeys.len(), 1);
    policy.author_allowlist_sha256 = bytes_sha256(&allowlist_bytes);

    super::super::super::persist_stage_segment(
        &staging_data_dir,
        &staged_segment(0, 1, 0, 0, 0, Vec::new()),
        &policy,
    )
    .expect("publish generated claimed stage segment");
    let staging_source = format!(
        "http://127.0.0.1:45441/eligible/{}",
        policy.author_allowlist_sha256
    );
    let stage_state = StagedNostrCrawlState {
        version: STAGE_FORMAT_VERSION,
        author_allowlist_source: Some(staging_source.clone()),
        policy: policy.clone(),
        next_author: 1,
        events_seen: 0,
        events_selected: 0,
        live_bytes_selected: 0,
    };
    super::super::super::persist_stage_state(&staging_data_dir, &stage_state)
        .expect("persist generated partial staging state");
    let stage_state_path = staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
    let stage_state_sha256 =
        bytes_sha256(&std::fs::read(&stage_state_path).expect("read generated staging state"));

    let mut built_roots = BTreeMap::new();
    for (position, index) in NostrEventIndex::ALL.into_iter().enumerate() {
        built_roots.insert(
            index.stable_id(),
            cid_to_nhash(&hashtree_core::Cid::public([position as u8 + 1; 32]))
                .expect("encode generated index root"),
        );
    }
    let candidate_cid = hashtree_core::Cid::public([240; 32]);
    let candidate_root = cid_to_nhash(&candidate_cid).expect("encode generated candidate root");
    let projection_source = format!(
        "http://127.0.0.1:45442/eligible/{}",
        policy.author_allowlist_sha256
    );
    assert_ne!(
        projection_source, staging_source,
        "the regression requires different ephemeral transports"
    );
    let v2_state = BulkProjectionState {
        version: BULK_PROJECTION_VERSION,
        author_allowlist_source: Some(projection_source),
        policy: policy.clone(),
        next_author: 1,
        segment_event_offset: 0,
        events_seen: 0,
        events_selected: 0,
        live_bytes_selected: 0,
        built_roots: built_roots.clone(),
        complete_root: Some(candidate_root.clone()),
    };
    let (v2_state_path, spool_path) = bulk_paths(&data_dir);
    super::super::persist_bulk_state(&v2_state_path, &v2_state)
        .expect("persist generated terminal v2 state");
    let v2_state_sha256 =
        bytes_sha256(&std::fs::read(&v2_state_path).expect("read generated terminal v2 state"));
    drop(BulkProjectionSpool::open(&spool_path).expect("create real spool LMDB"));

    let rank_source = source_dir.join("rank");
    std::fs::create_dir_all(&rank_source).expect("create generated rank source directory");
    let rank_authority =
        test_rank_authority_for_pubkeys(&rank_source, &pubkeys, &policy.author_allowlist_sha256);
    let policy_sha256 = bytes_sha256(
        &serde_json::to_vec(&policy).expect("serialize generated canonical crawl policy"),
    );
    let profile_by_pubkey_root =
        cid_to_nhash(&hashtree_core::Cid::public([241; 32])).expect("encode profile root");
    let profile_search_root =
        cid_to_nhash(&hashtree_core::Cid::public([242; 32])).expect("encode search root");
    let profile_distance_seal_sha256 = "8".repeat(64);
    let audit = AuditEvidenceFile {
        version: 3,
        subject_kind: AuditSubjectKind::V2,
        subject_version: BULK_PROJECTION_VERSION,
        candidate_root: candidate_root.clone(),
        state_sha256: v2_state_sha256.clone(),
        stage_state_sha256: stage_state_sha256.clone(),
        trusted_policy_sha256: policy_sha256,
        policy_author_allowlist_sha256: Some(policy.author_allowlist_sha256.clone()),
        trusted_profile_distance_seal_sha256: Some(profile_distance_seal_sha256.clone()),
        profile_distance_provenance: Some(rank_authority.evidence.clone()),
        trusted_full_author_count: pubkeys.len(),
        crawl_policy_max_follow_distance: policy.max_follow_distance,
        audit_mode: "recovery-tranche-internal-non-cutover".to_string(),
        cutover_eligible: false,
        pool_catalog_sha256: "4".repeat(64),
        pool_manifest_sha256: "5".repeat(64),
        pool_stored_locations: 1,
        authors_processed: 1,
        authors_total: pubkeys.len(),
        recovery_tranche_only: true,
        indexes: NostrEventIndex::ALL
            .into_iter()
            .map(|index| AuditIndexEvidence {
                index: index.name().to_string(),
                root: built_roots.get(&index.stable_id()).cloned(),
                nodes: 1,
                links: 0,
                durable_values_validated: 0,
                entries_sha256: "1".repeat(64),
                retained_set_sha256: "2".repeat(64),
                first_key: None,
                last_key: None,
            })
            .collect(),
        profile: AuditProfileEvidence {
            by_pubkey_root: profile_by_pubkey_root,
            by_pubkey_root_file_sha256: "6".repeat(64),
            by_pubkey_nodes: 1,
            by_pubkey_links: 0,
            by_pubkey_entries_sha256: "a".repeat(64),
            search_root: profile_search_root,
            search_root_file_sha256: "7".repeat(64),
            search_nodes: 1,
            search_entries: 1,
            search_entries_sha256: "b".repeat(64),
            sample_pubkey: pubkeys[0].clone(),
            sample_event_id: "c".repeat(64),
            sample_name: "generated profile".to_string(),
            follow_distance_binding: "rank-decisions".to_string(),
            follow_distance_seal_sha256: profile_distance_seal_sha256,
        },
        queries: vec![AuditQueryEvidence {
            query: "generated exact prepare regression".to_string(),
            parameters: serde_json::json!({"limit": 1}),
            event_ids: vec!["c".repeat(64)],
        }],
        representative_blocks: vec![AuditBlockEvidence {
            role: "candidate-root".to_string(),
            nhash: candidate_root.clone(),
            sha256: "d".repeat(64),
        }],
    };
    let audit_path = source_dir.join("audit.json");
    let mut audit_bytes = serde_json::to_vec_pretty(&audit).expect("encode generated audit");
    audit_bytes.push(b'\n');
    std::fs::write(&audit_path, audit_bytes).expect("write generated audit");

    let serving_keys = Keys::generate();
    let serving_tree_name = "social.iris.to";
    let candidate_hash = hex::encode(candidate_cid.hash);
    let serving_event = EventBuilder::new(Kind::Custom(HASHTREE_ROOT_KIND as u16), "")
        .tags([
            Tag::identifier(serving_tree_name),
            Tag::parse(["hash", candidate_hash.as_str()]).expect("build root hash tag"),
        ])
        .custom_created_at(Timestamp::from_secs(1))
        .sign_with_keys(&serving_keys)
        .expect("sign generated serving root event");
    let serving_event_path = source_dir.join("serving-event.json");
    std::fs::write(&serving_event_path, serving_event.as_json())
        .expect("write generated serving event");

    let output = prepare_bulk_tranche(
        &data_dir,
        BulkTranchePrepareOptions {
            staging_data_dir: staging_data_dir.clone(),
            eligible_authors: allowlist_path,
            expected_v2_state_sha256: v2_state_sha256,
            expected_stage_state_sha256: stage_state_sha256,
            audit_evidence: audit_path,
            profile_rank_decisions_file: rank_authority.decisions_path,
            expected_profile_rank_decisions_file_sha256: rank_authority
                .evidence
                .rank_decisions_file_sha256,
            profile_rank_decisions_report: rank_authority.report_path,
            expected_profile_rank_decisions_report_sha256: rank_authority
                .evidence
                .rank_decisions_report_sha256,
            serving_root: candidate_root,
            serving_event: serving_event_path,
            serving_event_id: serving_event.id.to_hex(),
            serving_publisher_pubkey: serving_keys.public_key().to_hex(),
            serving_tree_name: serving_tree_name.to_string(),
            btree_order: 8,
            btree_update_concurrency: 1,
            index_commit_batch_size: 1,
            out: Some(tmp.path().join("prepare-output.json")),
        },
    )
    .expect("Prepare must bind canonical policy rather than ephemeral source URL");
    assert_eq!(output.phase, "appending");
    assert_eq!(output.next_author, 1);
    assert_eq!(output.authors_total, pubkeys.len());

    let (state_path, seals_dir, _, _, _) = tranche_paths(&data_dir);
    let (persisted, _, persisted_sha256) = load_state(&state_path, &seals_dir)
        .expect("load generated v3 state")
        .expect("generated v3 state exists");
    assert_eq!(persisted_sha256, output.state_sha256);
    assert_eq!(persisted.phase, TranchePhase::Appending);
    assert_eq!(persisted.policy, policy);
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
    let events = [
        EventBuilder::new(Kind::TextNote, "append body read")
            .tags([Tag::parse(["t", "candidate-build"]).expect("build test tag")])
            .custom_created_at(Timestamp::from_secs(100))
            .sign_with_keys(&keys)
            .expect("sign generated text-note event"),
        EventBuilder::new(Kind::Metadata, r#"{"name":"candidate builder"}"#)
            .custom_created_at(Timestamp::from_secs(101))
            .sign_with_keys(&keys)
            .expect("sign generated profile event"),
        EventBuilder::new(Kind::Custom(30_023), "candidate long-form event")
            .tags([Tag::identifier("candidate-build")])
            .custom_created_at(Timestamp::from_secs(102))
            .sign_with_keys(&keys)
            .expect("sign generated parameterized-replaceable event"),
    ];
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
    let profile_by_pubkey_root =
        cid_to_nhash(&hashtree_core::Cid::public([241; 32])).expect("encode profile root");
    let profile_search_root =
        cid_to_nhash(&hashtree_core::Cid::public([242; 32])).expect("encode search root");
    let evidence = AuditEvidencePin {
        sha256: "1".repeat(64),
        audit_format_version: 3,
        subject_kind: AuditSubjectKind::V2,
        subject_version: BULK_PROJECTION_VERSION,
        candidate_root: candidate.root.clone(),
        subject_state_sha256: "2".repeat(64),
        stage_state_sha256: stage_state_sha256.clone(),
        trusted_policy_sha256: "3".repeat(64),
        policy_author_allowlist_sha256: policy.author_allowlist_sha256.clone(),
        trusted_full_author_count: 1,
        crawl_policy_max_follow_distance: Some(0),
        audit_mode: "recovery-tranche-internal-non-cutover".to_string(),
        cutover_eligible: false,
        authors_processed: 0,
        authors_total: 1,
        recovery_tranche_only: true,
        index_roots: candidate.built_roots.clone(),
        pool_catalog_sha256: "4".repeat(64),
        pool_manifest_sha256: "5".repeat(64),
        pool_stored_locations: 1,
        profile_by_pubkey_root,
        profile_by_pubkey_root_file_sha256: "6".repeat(64),
        profile_search_root,
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
    let mut unknown_seal = serde_json::to_value(&seal).expect("encode test seal value");
    unknown_seal
        .as_object_mut()
        .expect("seal object")
        .insert("unexpected".to_string(), serde_json::json!(true));
    serde_json::from_value::<TrancheSeal>(unknown_seal)
        .expect_err("tranche seals must deny unknown fields");
    let mut noncanonical_seal_bytes =
        serde_json::to_vec_pretty(&seal).expect("encode noncanonical test seal");
    noncanonical_seal_bytes.push(b'\n');
    let noncanonical_seal_sha256 = bytes_sha256(&noncanonical_seal_bytes);
    persist_immutable_bytes(
        &seal_path(&seals_dir, seal.generation, &noncanonical_seal_sha256),
        &noncanonical_seal_bytes,
        "noncanonical test seal",
    )
    .expect("persist noncanonical test seal");
    assert!(
        load_seal(&seals_dir, seal.generation, &noncanonical_seal_sha256)
            .expect_err("seal loader must reject noncanonical JSON")
            .to_string()
            .contains("not canonical JSON")
    );
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
    let state_sha256 =
        persist_state_cas(&state_path, &state, None).expect("persist Appending state");
    let canonical_state_bytes = std::fs::read(&state_path).expect("read canonical state");
    let stale_error = persist_state_cas(&state_path, &state, Some(&"f".repeat(64)))
        .expect_err("stale state compare-and-swap must fail");
    assert!(stale_error.to_string().contains("mismatch"));
    assert_eq!(
        std::fs::read(&state_path).expect("re-read state after rejected CAS"),
        canonical_state_bytes,
        "a rejected compare-and-swap must not replace durable state"
    );
    let noncanonical_state_path = state_path.with_file_name("noncanonical-state.json");
    let mut noncanonical_state_bytes =
        serde_json::to_vec_pretty(&state).expect("encode noncanonical state");
    noncanonical_state_bytes.push(b'\n');
    std::fs::write(&noncanonical_state_path, noncanonical_state_bytes)
        .expect("write noncanonical test state");
    assert!(load_state(&noncanonical_state_path, &seals_dir)
        .expect_err("state loader must reject noncanonical JSON")
        .to_string()
        .contains("not canonical JSON"));
    let mut unknown_state = serde_json::to_value(&state).expect("encode state value");
    unknown_state
        .as_object_mut()
        .expect("state object")
        .insert("unexpected".to_string(), serde_json::json!(true));
    serde_json::from_value::<BulkTrancheState>(unknown_state)
        .expect_err("tranche state must deny unknown fields");
    let mut unsupported_phase = state.clone();
    unsupported_phase.phase = TranchePhase::Building;
    unsupported_phase.generation = 2;
    assert!(validate_state_schema(&unsupported_phase)
        .expect_err("invalid Building phase states must fail closed")
        .to_string()
        .contains("violates its exact phase invariants"));
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
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: output.state_sha256,
            through_author: 1,
            out: None,
        },
    )
    .expect("freeze the fully appended generated prefix");
    let (frozen_state, _, frozen_state_sha256) = load_state(&state_path, &seals_dir)
        .expect("load frozen state")
        .expect("frozen state exists");
    assert_eq!(frozen_state_sha256, freeze_output.state_sha256);
    assert_eq!(frozen_state.phase, TranchePhase::Freeze);

    let building_output = build_bulk_tranche(
        &projection_store,
        &graph,
        &data_dir,
        BulkTrancheBuildOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: frozen_state_sha256.clone(),
            max_indexes: 1,
            out: None,
        },
    )
    .await
    .expect("build one real index and leave a crash-resumable Building state");
    assert_eq!(building_output.phase, "building");
    let stale_error = build_bulk_tranche(
        &projection_store,
        &graph,
        &data_dir,
        BulkTrancheBuildOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: frozen_state_sha256,
            max_indexes: NostrEventIndex::ALL.len(),
            out: None,
        },
    )
    .await
    .expect_err("a stale build invocation must not replace resumable progress");
    assert!(stale_error.to_string().contains("mismatch"));

    let candidate_output = build_bulk_tranche(
        &projection_store,
        &graph,
        &data_dir,
        BulkTrancheBuildOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: building_output.state_sha256,
            max_indexes: NostrEventIndex::ALL.len(),
            out: None,
        },
    )
    .await
    .expect("resume real sorted index construction through a terminal Candidate");
    assert_eq!(candidate_output.phase, "candidate");
    let (candidate_state, _, candidate_state_sha256) = load_state(&state_path, &seals_dir)
        .expect("load built candidate state")
        .expect("built candidate state exists");
    assert_eq!(candidate_state_sha256, candidate_output.state_sha256);
    assert_eq!(candidate_state.phase, TranchePhase::Candidate);
    assert_eq!(
        candidate_state.working.built_roots.len(),
        NostrEventIndex::ALL.len()
    );
    assert!(candidate_state
        .working
        .built_roots
        .values()
        .all(|root| !root.is_empty()));
    let candidate_root = parse_root_text(
        candidate_state
            .working
            .candidate_root
            .as_deref()
            .expect("built candidate root"),
    )
    .expect("parse built candidate root");
    let candidate_store = NostrEventStore::with_options(
        projection_store.store_arc(),
        NostrEventStoreOptions {
            btree_order: Some(8),
            btree_update_concurrency: Some(1),
            index_commit_batch_size: Some(1),
        },
    );
    candidate_store
        .validate_index_root(Some(&candidate_root))
        .await
        .expect("validate candidate through the production event-store manifest path");
    let recent = candidate_store
        .list_recent(
            Some(&candidate_root),
            hashtree_nostr::ListEventsOptions {
                limit: Some(events.len()),
                ..Default::default()
            },
        )
        .await
        .expect("query built candidate through the production event-store path");
    assert_eq!(recent.len(), events.len());

    let stage_state_path = staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
    let stage_state_bytes = std::fs::read(&stage_state_path).expect("read terminal staging state");
    let mut changed_stage_state_bytes = stage_state_bytes.clone();
    changed_stage_state_bytes.push(b'\n');
    std::fs::write(&stage_state_path, changed_stage_state_bytes)
        .expect("simulate changed terminal staging state");
    let changed_stage_error = build_bulk_tranche(
        &projection_store,
        &graph,
        &data_dir,
        BulkTrancheBuildOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: candidate_state_sha256.clone(),
            max_indexes: NostrEventIndex::ALL.len(),
            out: None,
        },
    )
    .await
    .expect_err("Candidate validation must reject a changed frozen staging state");
    assert!(changed_stage_error.to_string().contains("SHA-256 mismatch"));
    std::fs::write(&stage_state_path, stage_state_bytes).expect("restore terminal staging state");

    let repeated_candidate = build_bulk_tranche(
        &projection_store,
        &graph,
        &data_dir,
        BulkTrancheBuildOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: candidate_state_sha256.clone(),
            max_indexes: NostrEventIndex::ALL.len(),
            out: None,
        },
    )
    .await
    .expect("idempotently revalidate the persisted Candidate without changing its state");
    assert_eq!(repeated_candidate.phase, "candidate");
    assert_eq!(repeated_candidate.state_sha256, candidate_state_sha256);

    let authority =
        load_v3_candidate_audit_authority(&data_dir, &staging_data_dir, &candidate_state_sha256)
            .expect(
                "derive v3 audit authority from canonical Candidate state and Freeze seal chain",
            );
    assert_eq!(
        authority.candidate_root,
        candidate_state
            .working
            .candidate_root
            .clone()
            .expect("generated candidate root")
    );
    assert_eq!(authority.built_roots, candidate_state.working.built_roots);
    assert_eq!(authority.author_count, candidate_state.policy.author_count);
    assert_eq!(
        authority.policy_author_allowlist_sha256,
        candidate_state.policy.author_allowlist_sha256
    );
    assert!(
        load_v3_candidate_audit_authority(&data_dir, &staging_data_dir, &"f".repeat(64),)
            .expect_err("v3 audit authority must reject an unrelated state pin")
            .to_string()
            .contains("mismatch")
    );
    super::super::audit::load_v3_audit_subject(
        &data_dir,
        super::super::audit::V3AuditSubjectSpec {
            expected_state_sha256: candidate_state_sha256.clone(),
            staging_data_dir: staging_data_dir.clone(),
        },
    )
    .expect("load typed v3 audit subject without caller-supplied roots or counts");

    drop(candidate_store);
    drop(staging_events);
    drop(graph);
    drop(projection_store);
    drop(staging_store);
    // Heed caches opened environments process-wide. Evict the writer so this
    // same-process production-path test can exercise the auditor's strict
    // MDB_RDONLY spool open.
    let spool_to_close =
        BulkProjectionSpool::open(&spool_path).expect("reopen generated spool for closing");
    let spool_closing = spool_to_close.env.clone().prepare_for_closing();
    drop(spool_to_close);
    spool_closing.wait();

    let audit_path = tmp.path().join("candidate-audit.json");
    super::super::audit::audit_bulk_projection(
        &data_dir,
        super::super::audit::BulkProjectionAuditOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: candidate_state_sha256.clone(),
            v3_candidate: true,
            expected_stage_state_sha256: None,
            expected_policy_sha256: None,
            expected_profile_distance_seal_sha256: None,
            profile_rank_decisions_file: None,
            expected_profile_rank_decisions_file_sha256: None,
            profile_rank_decisions_report: None,
            expected_profile_rank_decisions_report_sha256: None,
            expected_full_author_count: None,
            allow_recovery_tranche: false,
            btree_order: 8,
            page_size: 8,
            query_limit: events.len(),
            out: audit_path.clone(),
        },
    )
    .await
    .expect("run the strict CLI audit path over the real generated v3 Candidate");
    let audit: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&audit_path).expect("read generated v3 Candidate audit evidence"),
    )
    .expect("parse generated v3 Candidate audit evidence");
    assert_eq!(audit["subject_kind"], "v3");
    assert_eq!(audit["subject_version"], 3);
    assert_eq!(audit["state_sha256"], candidate_state_sha256);
    assert_eq!(audit["audit_mode"], "full-policy-cutover");
    assert_eq!(audit["cutover_eligible"], true);
    assert_eq!(audit["recovery_tranche_only"], false);
    assert_eq!(audit["authors_processed"], 1);
    assert_eq!(audit["authors_total"], 1);
    assert_eq!(
        audit["candidate_root"],
        candidate_state
            .working
            .candidate_root
            .as_deref()
            .expect("generated candidate root")
    );
    assert_eq!(
        audit["indexes"]
            .as_array()
            .expect("v3 audit index evidence")
            .len(),
        NostrEventIndex::ALL.len()
    );
    assert!(!audit["queries"]
        .as_array()
        .expect("v3 audit query evidence")
        .is_empty());
    assert!(!audit["representative_blocks"]
        .as_array()
        .expect("v3 audit representative block evidence")
        .is_empty());

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
    let freeze_parent_sha256 = frozen_seal
        .parent_seal_sha256
        .clone()
        .expect("Freeze seal parent");
    let freeze_parent_path = seal_path(&seals_dir, 0, &freeze_parent_sha256);
    let freeze_parent_bytes =
        std::fs::read(&freeze_parent_path).expect("read canonical Prepare parent");
    let mut parentless_frozen_seal = frozen_seal;
    parentless_frozen_seal.parent_seal_sha256 = None;
    assert!(validate_active_seal(&frozen_state, &parentless_frozen_seal)
        .expect_err("Freeze seal must retain its exact parent link")
        .to_string()
        .contains("no parent Prepare seal"));
    std::fs::write(&freeze_parent_path, b"{\"substituted\":true}\n")
        .expect("substitute generated parent seal");
    assert!(load_state(&state_path, &seals_dir)
        .expect_err("restart must reject a substituted parent seal")
        .to_string()
        .contains("SHA-256 mismatch"));
    std::fs::write(&freeze_parent_path, &freeze_parent_bytes)
        .expect("restore canonical parent seal");
    std::fs::remove_file(&freeze_parent_path).expect("remove generated parent seal");
    assert!(load_state(&state_path, &seals_dir)
        .expect_err("restart must reject a missing parent seal")
        .to_string()
        .contains("read"));
}
