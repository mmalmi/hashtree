use super::*;

use tempfile::TempDir;

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
    let initial = attest_stage_prefix(tmp.path(), 2, 7, 2, 512, "11".repeat(32), &policy)
        .expect("attest first prefix");
    assert_eq!(initial.segment_count, 1);
    assert_eq!(initial.event_cid_count, 1);

    let second = staged_segment(2, 4, 3, 0, 0, Vec::new());
    super::super::super::persist_stage_segment(tmp.path(), &second, &policy)
        .expect("publish later real staged segment");
    let repeated = attest_stage_prefix(tmp.path(), 2, 7, 2, 512, "22".repeat(32), &policy)
        .expect("reattest immutable prefix after staging advances");
    assert!(initial.immutable_prefix_eq(&repeated));
    assert_ne!(
        initial.observed_stage_state_sha256,
        repeated.observed_stage_state_sha256
    );

    let extended = attest_stage_prefix(tmp.path(), 4, 10, 2, 512, "33".repeat(32), &policy)
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
    let sealed =
        attest_stage_prefix(tmp.path(), 1, 1, 0, 0, "44".repeat(32), &policy).expect("seal prefix");

    let mut mutated = segment;
    mutated.events_seen = 2;
    let path = super::super::super::stage_segment_path(tmp.path(), 0, 1);
    let mut bytes = serde_json::to_vec(&mutated).expect("encode mutation");
    bytes.push(b'\n');
    std::fs::write(&path, bytes).expect("simulate post-publish disk mutation");

    let observed = attest_stage_prefix(tmp.path(), 1, 2, 0, 0, "55".repeat(32), &policy)
        .expect("attest mutated bytes");
    assert!(!sealed.immutable_prefix_eq(&observed));
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

    let error = attest_stage_prefix(tmp.path(), 1, 0, 0, 0, "66".repeat(32), &policy)
        .expect_err("terminal full scan must reject duplicate starts");
    assert!(error.to_string().contains("duplicate staged segment start"));
}
