use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hashtree_nostr::{
    parse_verified_hashtree_root_event, NostrEventIndex, NostrEventStore, NostrEventStoreOptions,
};
use nostr::{Event, JsonUtil};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::{
    cid_to_nhash, load_stage_segment_claim, load_stage_segment_path_with_bytes,
    load_stage_segment_with_bytes, note_stage_segment_directory_scan, parse_author_allowlist,
    parse_root_text, persist_immutable_bytes, persist_json_atomic, persist_stage_segment_claim,
    stage_bytes_sha256, stage_segment_file_parts, validate_stage_state, IndexedNostrCrawlPolicy,
    ProjectionStores, StagedAuthorSegment, StagedNostrCrawlState, STAGE_DIR, STAGE_SEGMENTS_DIR,
    STAGE_SEGMENT_CLAIMS_DIR, STAGE_STATE_FILE,
};
use super::audit::{
    load_pinned_profile_rank_decisions, recheck_trusted_profile_rank_decisions,
    require_profile_rank_policy_binding, ProfileDistanceProvenance, TrustedProfileRankDecisions,
};
use super::{
    bulk_paths, replay_staged_event_chunk, BulkProjectionSpool, BulkProjectionState,
    ProfileDistanceAuthority, BULK_PROJECTION_VERSION,
};

const TRANCHE_STATE_VERSION: u32 = 3;
const TRANCHE_DIR: &str = "bulk-projection-v3";
const TRANCHE_STATE_FILE: &str = "state.json";
const TRANCHE_SEALS_DIR: &str = "seals";
const TRANCHE_EVIDENCE_DIR: &str = "evidence";
const TRANCHE_SERVING_EVENTS_DIR: &str = "serving-events";
const TRANCHE_SPOOL_IDENTITY_FILE: &str = "spool-identity.json";
const PROFILE_PUBLICATION_FENCE_BYTES: &[u8] =
    b"{\"format\":\"hashtree-profile-publication-fence\",\"version\":3}\n";
const PREFIX_CHAIN_DOMAIN: &[u8] = b"hashtree-nostr-tranche-prefix-chain-v3\0";
const CID_CHAIN_DOMAIN: &[u8] = b"hashtree-nostr-tranche-prefix-cid-chain-v3\0";
const CHAIN_SEED_SUFFIX: &[u8] = b"seed\0";
const SPOOL_IDENTITY_DOMAIN: &[u8] = b"hashtree-nostr-tranche-spool-v3\0";

#[derive(Debug, Clone)]
pub(crate) struct BulkTranchePrepareOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) eligible_authors: PathBuf,
    pub(crate) expected_v2_state_sha256: String,
    pub(crate) expected_stage_state_sha256: String,
    pub(crate) audit_evidence: PathBuf,
    pub(crate) profile_rank_decisions_file: PathBuf,
    pub(crate) expected_profile_rank_decisions_file_sha256: String,
    pub(crate) profile_rank_decisions_report: PathBuf,
    pub(crate) expected_profile_rank_decisions_report_sha256: String,
    pub(crate) serving_root: String,
    pub(crate) serving_event: PathBuf,
    pub(crate) serving_event_id: String,
    pub(crate) serving_publisher_pubkey: String,
    pub(crate) serving_tree_name: String,
    pub(crate) btree_order: usize,
    pub(crate) btree_update_concurrency: usize,
    pub(crate) index_commit_batch_size: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct BulkTrancheAppendOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: String,
    pub(crate) max_segments: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct BulkTrancheFreezeOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: String,
    pub(crate) through_author: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum TranchePhase {
    Prepare,
    Appending,
    Freeze,
    Building,
    Candidate,
    Verified,
    Publishing,
    Promoted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum TrancheSealPurpose {
    Prepare,
    Freeze,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StagePrefixSeal {
    next_author: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
    segment_count: usize,
    event_cid_count: usize,
    segment_chain_sha256: String,
    event_cid_chain_sha256: String,
    observed_stage_state_sha256: String,
}

impl StagePrefixSeal {
    fn empty(observed_stage_state_sha256: String) -> Self {
        Self {
            next_author: 0,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
            segment_count: 0,
            event_cid_count: 0,
            segment_chain_sha256: chain_seed(PREFIX_CHAIN_DOMAIN),
            event_cid_chain_sha256: chain_seed(CID_CHAIN_DOMAIN),
            observed_stage_state_sha256,
        }
    }

    fn immutable_prefix_eq(&self, other: &Self) -> bool {
        self.next_author == other.next_author
            && self.events_seen == other.events_seen
            && self.events_selected == other.events_selected
            && self.live_bytes_selected == other.live_bytes_selected
            && self.segment_count == other.segment_count
            && self.event_cid_count == other.event_cid_count
            && self.segment_chain_sha256 == other.segment_chain_sha256
            && self.event_cid_chain_sha256 == other.event_cid_chain_sha256
    }
}

#[derive(Debug, Clone, Copy)]
struct StagePrefixTarget {
    boundary: usize,
    durable_next_author: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SpoolIdentity {
    id: String,
    marker_sha256: String,
    canonical_spool_path: String,
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SpoolIdentityMarker {
    version: u32,
    id: String,
    canonical_spool_path: String,
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ServingRootPin {
    root: String,
    event_id: String,
    event_sha256: String,
    event_pubkey: String,
    event_created_at: u64,
    tree_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CandidatePin {
    root: String,
    built_roots: BTreeMap<u8, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AuditEvidencePin {
    sha256: String,
    candidate_root: String,
    v2_state_sha256: String,
    stage_state_sha256: String,
    trusted_policy_sha256: String,
    trusted_full_author_count: usize,
    pool_catalog_sha256: String,
    pool_manifest_sha256: String,
    profile_by_pubkey_root_file_sha256: String,
    profile_search_root_file_sha256: String,
    profile_follow_distance_seal_sha256: String,
    profile_distance_provenance: ProfileDistanceProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FrozenProfileDistanceSeal {
    sha256: String,
    retained_profile_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PublicationIntent {
    event_id: String,
    event_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PublicationReceipt {
    errors: usize,
    relay_event_ids: BTreeMap<String, String>,
    resolver_event_id: String,
    resolver_root: String,
    receipt_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkingProjection {
    next_author: usize,
    segment_event_offset: usize,
    active_segment_sha256: Option<String>,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
    rolling_prefix: StagePrefixSeal,
    frozen_profile_distances: Option<FrozenProfileDistanceSeal>,
    built_roots: BTreeMap<u8, String>,
    candidate_root: Option<String>,
    frozen_prefix: Option<StagePrefixSeal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BulkTrancheState {
    version: u32,
    phase: TranchePhase,
    generation: u64,
    policy: IndexedNostrCrawlPolicy,
    ordered_allowlist_sha256: String,
    ordered_allowlist_count: usize,
    active_seal_sha256: Option<String>,
    pending_seal_sha256: Option<String>,
    serving: ServingRootPin,
    last_validated: CandidatePin,
    last_evidence: AuditEvidencePin,
    profile_rank_authority: ProfileDistanceProvenance,
    spool_identity: SpoolIdentity,
    btree_order: usize,
    btree_update_concurrency: usize,
    index_commit_batch_size: usize,
    working: WorkingProjection,
    publication_intent: Option<PublicationIntent>,
    publication_receipt: Option<PublicationReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrancheSeal {
    version: u32,
    generation: u64,
    parent_seal_sha256: Option<String>,
    purpose: TrancheSealPurpose,
    policy: IndexedNostrCrawlPolicy,
    ordered_allowlist_sha256: String,
    ordered_allowlist_count: usize,
    prefix: StagePrefixSeal,
    spool_identity: SpoolIdentity,
    internal_candidate: Option<CandidatePin>,
    evidence: Option<AuditEvidencePin>,
    profile_rank_authority: ProfileDistanceProvenance,
    frozen_profile_distances: Option<FrozenProfileDistanceSeal>,
    serving: ServingRootPin,
    publication_intent: Option<PublicationIntent>,
    publication_receipt: Option<PublicationReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AuditEvidenceFile {
    version: u32,
    candidate_root: String,
    state_sha256: String,
    stage_state_sha256: String,
    trusted_policy_sha256: String,
    trusted_profile_distance_seal_sha256: Option<String>,
    profile_distance_provenance: Option<ProfileDistanceProvenance>,
    trusted_full_author_count: usize,
    crawl_policy_max_follow_distance: Option<u32>,
    audit_mode: String,
    cutover_eligible: bool,
    pool_catalog_sha256: String,
    pool_manifest_sha256: String,
    pool_stored_locations: u64,
    authors_processed: usize,
    authors_total: usize,
    recovery_tranche_only: bool,
    indexes: Vec<AuditIndexEvidence>,
    profile: AuditProfileEvidence,
    queries: Vec<AuditQueryEvidence>,
    representative_blocks: Vec<AuditBlockEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AuditIndexEvidence {
    index: String,
    root: Option<String>,
    nodes: u64,
    links: u64,
    durable_values_validated: u64,
    entries_sha256: String,
    retained_set_sha256: String,
    first_key: Option<String>,
    last_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AuditProfileEvidence {
    by_pubkey_root: String,
    by_pubkey_root_file_sha256: String,
    by_pubkey_nodes: u64,
    by_pubkey_links: u64,
    by_pubkey_entries_sha256: String,
    search_root: String,
    search_root_file_sha256: String,
    search_nodes: u64,
    search_entries: u64,
    search_entries_sha256: String,
    sample_pubkey: String,
    sample_event_id: String,
    sample_name: String,
    follow_distance_binding: String,
    follow_distance_seal_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AuditQueryEvidence {
    query: String,
    parameters: serde_json::Value,
    event_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AuditBlockEvidence {
    role: String,
    nhash: String,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BulkTrancheTransitionOutput {
    pub(crate) version: u32,
    pub(crate) phase: String,
    pub(crate) generation: u64,
    pub(crate) state_sha256: String,
    pub(crate) active_seal_sha256: Option<String>,
    pub(crate) next_author: usize,
    pub(crate) authors_total: usize,
}

fn tranche_paths(data_dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let base = data_dir.join(super::super::INDEX_DIR).join(TRANCHE_DIR);
    (
        base.join(TRANCHE_STATE_FILE),
        base.join(TRANCHE_SEALS_DIR),
        base.join(TRANCHE_EVIDENCE_DIR),
        base.join(TRANCHE_SERVING_EVENTS_DIR),
        base.join(TRANCHE_SPOOL_IDENTITY_FILE),
    )
}

fn profile_rank_evidence_paths(
    evidence_dir: &Path,
    provenance: &ProfileDistanceProvenance,
) -> (PathBuf, PathBuf) {
    (
        evidence_dir
            .join("profile-rank-decisions")
            .join(format!("{}.jsonl", provenance.rank_decisions_file_sha256)),
        evidence_dir
            .join("profile-rank-reports")
            .join(format!("{}.json", provenance.rank_decisions_report_sha256)),
    )
}

fn ordered_allowlist_evidence_path(evidence_dir: &Path, sha256: &str) -> PathBuf {
    evidence_dir
        .join("ordered-allowlists")
        .join(format!("{sha256}.txt"))
}

fn validate_profile_publication_fence(data_dir: &Path) -> Result<()> {
    let path = hashtree_cli::socialgraph::profile_publication_fence_path(data_dir);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read profile publication fence {}", path.display()))?;
    if bytes != PROFILE_PUBLICATION_FENCE_BYTES {
        anyhow::bail!(
            "profile publication fence {} differs from the v3 durable marker",
            path.display()
        );
    }
    Ok(())
}

fn load_copied_profile_rank_authority(
    evidence_dir: &Path,
    expected: &ProfileDistanceProvenance,
) -> Result<TrustedProfileRankDecisions> {
    let (decisions_path, report_path) = profile_rank_evidence_paths(evidence_dir, expected);
    let decisions_path = decisions_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", decisions_path.display()))?;
    let report_path = report_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", report_path.display()))?;
    let authority = load_pinned_profile_rank_decisions(
        &decisions_path,
        &expected.rank_decisions_file_sha256,
        &report_path,
        &expected.rank_decisions_report_sha256,
    )?;
    if authority.evidence != *expected {
        anyhow::bail!("copied profile rank authority differs from durable v3 provenance");
    }
    Ok(authority)
}

fn bytes_sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn validate_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} must be a lowercase 64-character SHA-256");
    }
    Ok(())
}

fn validate_lower_hex_32(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} must be lowercase 32-byte hex");
    }
    Ok(())
}

fn require_sha256(label: &str, actual: &str, expected: &str) -> Result<()> {
    validate_sha256(label, expected)?;
    if actual != expected {
        anyhow::bail!("{label} mismatch: expected {expected}, found {actual}");
    }
    Ok(())
}

fn json_line<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).with_context(|| format!("encode {label}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn load_ordered_allowlist(path: &Path, policy: &IndexedNostrCrawlPolicy) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read ordered author allowlist {}", path.display()))?;
    let authors = parse_author_allowlist(&text, usize::MAX);
    let mut digest = Sha256::new();
    for author in &authors {
        digest.update(author.as_bytes());
        digest.update(b"\n");
    }
    let sha256 = hex::encode(digest.finalize());
    if authors.len() != policy.author_count || sha256 != policy.author_allowlist_sha256 {
        anyhow::bail!("ordered author allowlist does not match the v2 crawl policy");
    }
    Ok(authors)
}

fn canonical_ordered_allowlist_bytes(authors: &[String]) -> Vec<u8> {
    let capacity = authors
        .iter()
        .map(|author| author.len().saturating_add(1))
        .sum();
    let mut bytes = Vec::with_capacity(capacity);
    for author in authors {
        bytes.extend_from_slice(author.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn chain_seed(domain: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(CHAIN_SEED_SUFFIX);
    hex::encode(digest.finalize())
}

fn chain_step(domain: &[u8], previous: &str, fields: &[&[u8]]) -> Result<String> {
    validate_sha256("rolling prefix chain head", previous)?;
    let previous = hex::decode(previous).context("decode rolling prefix chain head")?;
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(previous);
    for field in fields {
        let field_len =
            u64::try_from(field.len()).context("rolling prefix field length exceeds u64")?;
        digest.update(field_len.to_be_bytes());
        digest.update(field);
    }
    Ok(hex::encode(digest.finalize()))
}

fn extend_stage_prefix(
    prefix: &mut StagePrefixSeal,
    path: &Path,
    bytes: &[u8],
    segment: &StagedAuthorSegment,
    observed_stage_state_sha256: &str,
) -> Result<()> {
    if segment.start_author != prefix.next_author {
        anyhow::bail!(
            "cannot extend rolling staged prefix at {} with segment {}..{}",
            prefix.next_author,
            segment.start_author,
            segment.end_author
        );
    }
    validate_sha256(
        "observed staging state SHA-256",
        observed_stage_state_sha256,
    )?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("staged segment path has no UTF-8 file name")?;
    prefix.segment_chain_sha256 = chain_step(
        PREFIX_CHAIN_DOMAIN,
        &prefix.segment_chain_sha256,
        &[file_name.as_bytes(), bytes],
    )?;
    for cid in &segment.event_cids {
        prefix.event_cid_chain_sha256 = chain_step(
            CID_CHAIN_DOMAIN,
            &prefix.event_cid_chain_sha256,
            &[cid.as_bytes()],
        )?;
    }
    prefix.next_author = segment.end_author;
    prefix.events_seen = prefix
        .events_seen
        .checked_add(segment.events_seen)
        .context("staged prefix events-seen overflow")?;
    prefix.events_selected = prefix
        .events_selected
        .checked_add(segment.events_selected)
        .context("staged prefix events-selected overflow")?;
    prefix.live_bytes_selected = prefix
        .live_bytes_selected
        .checked_add(segment.live_bytes_selected)
        .context("staged prefix live-byte overflow")?;
    prefix.segment_count = prefix
        .segment_count
        .checked_add(1)
        .context("staged prefix segment count overflow")?;
    prefix.event_cid_count = prefix
        .event_cid_count
        .checked_add(segment.event_cids.len())
        .context("staged prefix event CID count overflow")?;
    prefix.observed_stage_state_sha256 = observed_stage_state_sha256.to_string();
    Ok(())
}

/// Scan the immutable segment namespace once and validate its canonical
/// deterministic boundaries. Bodies are deliberately not retained here:
/// Prepare/Freeze stream only the exact `[0, boundary)` prefix below, while
/// Appending never calls this catalog scan.
fn stage_segment_body_catalog(
    staging_data_dir: &Path,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<BTreeMap<usize, PathBuf>> {
    let directory = staging_data_dir.join(STAGE_DIR).join(STAGE_SEGMENTS_DIR);
    note_stage_segment_directory_scan();
    let mut grouped = BTreeMap::<usize, Vec<(usize, PathBuf)>>::new();
    for entry in
        std::fs::read_dir(&directory).with_context(|| format!("read {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", directory.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            anyhow::bail!("staged segment directory contains a non-UTF-8 file name");
        };
        if !name.ends_with(".json") {
            continue;
        }
        let (file_start, file_end) = stage_segment_file_parts(&path)?;
        grouped
            .entry(file_start)
            .or_default()
            .push((file_end, path));
    }

    let mut segments = BTreeMap::new();
    for (start, entries) in grouped {
        if entries.len() != 1 {
            let paths = entries
                .iter()
                .map(|(_, path)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("duplicate staged segment start {start} in immutable catalog: {paths}");
        }
        let (end, path) = entries.into_iter().next().expect("one catalog entry");
        if end <= start || end > policy.author_count {
            anyhow::bail!(
                "staged segment catalog path {} has invalid boundary {start}..{end}",
                path.display()
            );
        }
        segments.insert(start, path);
    }
    Ok(segments)
}

fn backfill_stage_segment_claims(
    staging_data_dir: &Path,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<()> {
    for (start, path) in stage_segment_body_catalog(staging_data_dir, policy)? {
        let (_, bytes, segment) = load_stage_segment_path_with_bytes(&path, start, policy)?;
        persist_stage_segment_claim(staging_data_dir, &segment, &bytes)?;
    }
    Ok(())
}

fn stage_segment_catalog(
    staging_data_dir: &Path,
    policy: &IndexedNostrCrawlPolicy,
    durable_next_author: usize,
) -> Result<BTreeMap<usize, PathBuf>> {
    if durable_next_author > policy.author_count {
        anyhow::bail!(
            "durable staging cursor {durable_next_author} exceeds {} policy authors",
            policy.author_count
        );
    }
    let segments = stage_segment_body_catalog(staging_data_dir, policy)?;
    let claims_dir = staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_SEGMENT_CLAIMS_DIR);
    if segments.is_empty() && !claims_dir.exists() {
        return Ok(segments);
    }
    if !claims_dir.exists() {
        anyhow::bail!(
            "immutable staged segment bodies and per-start boundary claims differ; refusing terminal scan"
        );
    }
    let mut claimed_starts = BTreeMap::new();
    for entry in
        std::fs::read_dir(&claims_dir).with_context(|| format!("read {}", claims_dir.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", claims_dir.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("read file type for {}", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("staged segment claim directory contains a non-UTF-8 file name")?;
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.len() != 12 || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
            anyhow::bail!("staged segment claim file `{name}` has a non-canonical start");
        }
        let start = stem
            .parse::<usize>()
            .with_context(|| format!("parse staged segment claim start in `{name}`"))?;
        if claimed_starts.insert(start, path).is_some() {
            anyhow::bail!("duplicate staged segment claim start {start}");
        }
    }
    if claimed_starts.len() != segments.len() || claimed_starts.keys().ne(segments.keys()) {
        anyhow::bail!(
            "immutable staged segment bodies and per-start boundary claims differ; refusing terminal scan"
        );
    }
    for (start, path) in &segments {
        let claim = load_stage_segment_claim(staging_data_dir, *start)?;
        let (_, end) = stage_segment_file_parts(path)?;
        if claim.end_author != end {
            anyhow::bail!(
                "staged segment claim {}..{} differs from body path {}",
                claim.start_author,
                claim.end_author,
                path.display()
            );
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("read claimed body {}", path.display()))?;
        if stage_bytes_sha256(&bytes) != claim.body_sha256 {
            anyhow::bail!(
                "staged segment body {} differs from its immutable per-start claim",
                path.display()
            );
        }
    }
    let mut cursor = 0usize;
    for (start, path) in &segments {
        if *start != cursor {
            anyhow::bail!(
                "staged segment catalog is not one contiguous non-overlapping chain: expected start {cursor}, found {start} at {}",
                path.display()
            );
        }
        let (_, end) = stage_segment_file_parts(path)?;
        if end > durable_next_author {
            anyhow::bail!(
                "staged segment catalog contains uncheckpointed body {} beyond durable cursor {durable_next_author}",
                path.display()
            );
        }
        cursor = end;
    }
    if cursor != durable_next_author {
        anyhow::bail!(
            "staged segment catalog ends at {cursor}, not durable staging cursor {durable_next_author}"
        );
    }
    Ok(segments)
}

fn attest_stage_prefix(
    staging_data_dir: &Path,
    target: StagePrefixTarget,
    observed_stage_state_sha256: String,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<StagePrefixSeal> {
    if target.boundary > target.durable_next_author {
        anyhow::bail!(
            "staged attestation boundary {} exceeds durable staging cursor {}",
            target.boundary,
            target.durable_next_author
        );
    }
    let segments = stage_segment_catalog(staging_data_dir, policy, target.durable_next_author)?;
    let mut prefix = StagePrefixSeal::empty(observed_stage_state_sha256.clone());
    while prefix.next_author < target.boundary {
        let expected_path = segments.get(&prefix.next_author).with_context(|| {
            format!(
                "staged prefix is missing segment beginning at {}",
                prefix.next_author
            )
        })?;
        let (path, bytes, segment) =
            load_stage_segment_path_with_bytes(expected_path, prefix.next_author, policy)?;
        if &path != expected_path {
            anyhow::bail!(
                "targeted staged segment path {} differs from catalog path {}",
                path.display(),
                expected_path.display()
            );
        }
        if segment.end_author > target.boundary {
            anyhow::bail!(
                "chosen boundary {} falls inside staged segment {}..{}",
                target.boundary,
                segment.start_author,
                segment.end_author
            );
        }
        extend_stage_prefix(
            &mut prefix,
            &path,
            &bytes,
            &segment,
            &observed_stage_state_sha256,
        )?;
    }
    if prefix.next_author != target.boundary
        || prefix.events_seen != target.events_seen
        || prefix.events_selected != target.events_selected
        || prefix.live_bytes_selected != target.live_bytes_selected
    {
        anyhow::bail!(
            "staged prefix counters through {} differ from expected projection counters: \
             expected=({},{},{}) \
             actual=({},{},{})",
            target.boundary,
            target.events_seen,
            target.events_selected,
            target.live_bytes_selected,
            prefix.events_seen,
            prefix.events_selected,
            prefix.live_bytes_selected
        );
    }
    Ok(prefix)
}

fn filesystem_identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok((0, 0))
    }
}

fn spool_identity(data_dir: &Path) -> Result<(SpoolIdentity, Vec<u8>)> {
    let (_, spool_path) = bulk_paths(data_dir);
    let canonical_spool = std::fs::canonicalize(&spool_path)
        .with_context(|| format!("canonicalize spool {}", spool_path.display()))?;
    let data_file = canonical_spool.join("data.mdb");
    let (device, inode) = filesystem_identity(&data_file)?;
    let canonical_spool_path = canonical_spool
        .to_str()
        .context("canonical spool path is not UTF-8")?
        .to_string();
    let mut identity_digest = Sha256::new();
    identity_digest.update(SPOOL_IDENTITY_DOMAIN);
    identity_digest.update((canonical_spool_path.len() as u64).to_be_bytes());
    identity_digest.update(canonical_spool_path.as_bytes());
    identity_digest.update(device.to_be_bytes());
    identity_digest.update(inode.to_be_bytes());
    let id = hex::encode(identity_digest.finalize());
    let marker = SpoolIdentityMarker {
        version: TRANCHE_STATE_VERSION,
        id: id.clone(),
        canonical_spool_path: canonical_spool_path.clone(),
        device,
        inode,
    };
    let bytes = json_line(&marker, "spool identity marker")?;
    let marker_sha256 = bytes_sha256(&bytes);
    Ok((
        SpoolIdentity {
            id,
            marker_sha256,
            canonical_spool_path,
            device,
            inode,
        },
        bytes,
    ))
}

fn validate_spool_identity(
    data_dir: &Path,
    marker_path: &Path,
    expected: &SpoolIdentity,
) -> Result<()> {
    let bytes = std::fs::read(marker_path)
        .with_context(|| format!("read spool identity {}", marker_path.display()))?;
    require_sha256(
        "spool identity marker SHA-256",
        &bytes_sha256(&bytes),
        &expected.marker_sha256,
    )?;
    let marker: SpoolIdentityMarker =
        serde_json::from_slice(&bytes).context("parse spool identity marker")?;
    if marker.version != TRANCHE_STATE_VERSION
        || marker.id != expected.id
        || marker.canonical_spool_path != expected.canonical_spool_path
        || marker.device != expected.device
        || marker.inode != expected.inode
    {
        anyhow::bail!("spool identity marker differs from v3 state");
    }
    let (_, spool_path) = bulk_paths(data_dir);
    let canonical_spool = std::fs::canonicalize(&spool_path)
        .with_context(|| format!("canonicalize spool {}", spool_path.display()))?;
    let (device, inode) = filesystem_identity(&canonical_spool.join("data.mdb"))?;
    if canonical_spool.to_string_lossy() != marker.canonical_spool_path
        || device != marker.device
        || inode != marker.inode
    {
        anyhow::bail!("bulk spool filesystem identity changed after v3 preparation");
    }
    Ok(())
}

fn serving_root_pin(options: &BulkTranchePrepareOptions) -> Result<(ServingRootPin, Vec<u8>)> {
    let requested_root =
        parse_root_text(&options.serving_root).context("parse externally resolved serving root")?;
    let root = cid_to_nhash(&requested_root)?;
    let bytes = std::fs::read(&options.serving_event).with_context(|| {
        format!(
            "read externally resolved serving event {}",
            options.serving_event.display()
        )
    })?;
    let event =
        Event::from_json(std::str::from_utf8(&bytes).context("serving event file is not UTF-8")?)
            .context("parse externally resolved signed serving event")?;
    let event_id = event.id.to_hex();
    if event_id != options.serving_event_id {
        anyhow::bail!(
            "serving event id mismatch: expected {}, found {event_id}",
            options.serving_event_id
        );
    }
    validate_lower_hex_32(
        "authoritative serving publisher pubkey",
        &options.serving_publisher_pubkey,
    )?;
    let event_pubkey = event.pubkey.to_hex();
    if event_pubkey != options.serving_publisher_pubkey {
        anyhow::bail!(
            "serving event publisher mismatch: expected {}, found {event_pubkey}",
            options.serving_publisher_pubkey
        );
    }
    let parsed = parse_verified_hashtree_root_event(&event)
        .context("verify externally resolved serving root event")?
        .context("serving event is not a Hashtree root event")?;
    if parsed.tree_name != options.serving_tree_name {
        anyhow::bail!(
            "serving event tree mismatch: expected `{}`, found `{}`",
            options.serving_tree_name,
            parsed.tree_name
        );
    }
    if parsed.root_cid != requested_root {
        anyhow::bail!("serving event does not resolve to the explicitly supplied root");
    }
    Ok((
        ServingRootPin {
            root,
            event_id,
            event_sha256: bytes_sha256(&bytes),
            event_pubkey,
            event_created_at: parsed.event.created_at,
            tree_name: parsed.tree_name,
        },
        bytes,
    ))
}

fn validate_v2_terminal_state(state: &BulkProjectionState) -> Result<CandidatePin> {
    if state.version != BULK_PROJECTION_VERSION {
        anyhow::bail!("v3 preparation requires bulk projection state version 2");
    }
    if state.segment_event_offset != 0 {
        anyhow::bail!("v2 terminal projection stops inside a staged segment");
    }
    if state.built_roots.len() != NostrEventIndex::ALL.len() {
        anyhow::bail!("v2 terminal state does not contain exactly nine built roots");
    }
    for index in NostrEventIndex::ALL {
        let root = state
            .built_roots
            .get(&index.stable_id())
            .with_context(|| format!("v2 terminal state omitted {} root", index.name()))?;
        if root.is_empty() {
            anyhow::bail!("v2 terminal state has an empty {} root", index.name());
        }
        parse_root_text(root)
            .with_context(|| format!("parse v2 terminal {} root", index.name()))?;
    }
    let root = state
        .complete_root
        .as_deref()
        .context("v2 terminal state has no complete candidate root")
        .and_then(parse_root_text)
        .and_then(|root| cid_to_nhash(&root))?;
    Ok(CandidatePin {
        root,
        built_roots: state.built_roots.clone(),
    })
}

fn validate_stage_covers_v2_terminal(
    stage: &StagedNostrCrawlState,
    state: &BulkProjectionState,
    require_exact_terminal_state: bool,
) -> Result<()> {
    validate_stage_state(stage, &state.policy, state.policy.author_count)?;
    if state.author_allowlist_source != stage.author_allowlist_source {
        anyhow::bail!("staging policy does not match the v2 terminal projection");
    }
    if require_exact_terminal_state {
        if state.next_author != stage.next_author
            || state.events_seen != stage.events_seen
            || state.events_selected != stage.events_selected
            || state.live_bytes_selected != stage.live_bytes_selected
        {
            anyhow::bail!("v2 terminal projection and staging states do not match exactly");
        }
    } else if stage.next_author < state.next_author
        || stage.events_seen < state.events_seen
        || stage.events_selected < state.events_selected
        || stage.live_bytes_selected < state.live_bytes_selected
    {
        anyhow::bail!("current staging state no longer covers the pinned v2 terminal prefix");
    }
    Ok(())
}

fn accept_audit_evidence(
    bytes: &[u8],
    evidence_sha256: String,
    candidate: &CandidatePin,
    state_sha256: &str,
    stage_sha256: &str,
    state: &BulkProjectionState,
    profile_rank_authority: &ProfileDistanceProvenance,
) -> Result<AuditEvidencePin> {
    let evidence: AuditEvidenceFile =
        serde_json::from_slice(bytes).context("parse exhaustive audit evidence")?;
    let policy_sha256 = bytes_sha256(
        &serde_json::to_vec(&state.policy).context("serialize pinned v2 crawl policy")?,
    );
    if state.next_author >= state.policy.author_count {
        anyhow::bail!("v3 recovery Prepare requires a partial audited v2 recovery tranche");
    }
    if evidence.version != 2
        || evidence.candidate_root != candidate.root
        || evidence.state_sha256 != state_sha256
        || evidence.stage_state_sha256 != stage_sha256
        || evidence.trusted_policy_sha256 != policy_sha256
        || evidence.trusted_full_author_count != state.policy.author_count
        || evidence.crawl_policy_max_follow_distance != state.policy.max_follow_distance
        || evidence.authors_processed != state.next_author
        || evidence.authors_total != state.policy.author_count
        || !evidence.recovery_tranche_only
        || evidence.audit_mode != "recovery-tranche-internal-non-cutover"
        || evidence.cutover_eligible
        || evidence.profile_distance_provenance.as_ref() != Some(profile_rank_authority)
    {
        anyhow::bail!(
            "audit evidence does not pin the exact v2 state, full policy, profile-rank authority, and non-cutover recovery mode"
        );
    }
    if evidence
        .trusted_profile_distance_seal_sha256
        .as_deref()
        .is_some_and(|seal| seal != evidence.profile.follow_distance_seal_sha256)
    {
        anyhow::bail!(
            "audit evidence trusted profile-distance seal differs from its exhaustive profile audit"
        );
    }
    let mut roots = BTreeMap::new();
    for index in evidence.indexes {
        for (label, digest) in [
            ("entries", index.entries_sha256.as_str()),
            ("retained set", index.retained_set_sha256.as_str()),
        ] {
            validate_sha256(&format!("audit {} {label} SHA-256", index.index), digest)?;
        }
        if roots.insert(index.index.clone(), index.root).is_some() {
            anyhow::bail!("audit evidence repeats index `{}`", index.index);
        }
    }
    if roots.len() != NostrEventIndex::ALL.len() {
        anyhow::bail!("audit evidence does not contain exactly nine indexes");
    }
    for index in NostrEventIndex::ALL {
        let actual = roots
            .remove(index.name())
            .with_context(|| format!("audit evidence omitted {} index", index.name()))?
            .context("audit evidence contains an empty canonical index root")?;
        let expected = candidate
            .built_roots
            .get(&index.stable_id())
            .expect("candidate roots validated");
        if &actual != expected {
            anyhow::bail!("audit evidence {} root differs from v2 state", index.name());
        }
    }
    validate_sha256(
        "audit PoolStore catalog SHA-256",
        &evidence.pool_catalog_sha256,
    )?;
    validate_sha256(
        "audit PoolStore manifest SHA-256",
        &evidence.pool_manifest_sha256,
    )?;
    validate_sha256(
        "audit profile-by-pubkey root-file SHA-256",
        &evidence.profile.by_pubkey_root_file_sha256,
    )?;
    validate_sha256(
        "audit profile-search root-file SHA-256",
        &evidence.profile.search_root_file_sha256,
    )?;
    for (label, digest) in [
        (
            "audit profile-by-pubkey entries SHA-256",
            evidence.profile.by_pubkey_entries_sha256.as_str(),
        ),
        (
            "audit profile-search entries SHA-256",
            evidence.profile.search_entries_sha256.as_str(),
        ),
        (
            "audit profile follow-distance seal SHA-256",
            evidence.profile.follow_distance_seal_sha256.as_str(),
        ),
    ] {
        validate_sha256(label, digest)?;
    }
    for block in &evidence.representative_blocks {
        validate_sha256(
            &format!("audit representative block `{}` SHA-256", block.role),
            &block.sha256,
        )?;
        parse_root_text(&block.nhash)
            .with_context(|| format!("parse audit representative block `{}` nhash", block.role))?;
    }
    Ok(AuditEvidencePin {
        sha256: evidence_sha256,
        candidate_root: candidate.root.clone(),
        v2_state_sha256: state_sha256.to_string(),
        stage_state_sha256: stage_sha256.to_string(),
        trusted_policy_sha256: policy_sha256,
        trusted_full_author_count: evidence.trusted_full_author_count,
        pool_catalog_sha256: evidence.pool_catalog_sha256,
        pool_manifest_sha256: evidence.pool_manifest_sha256,
        profile_by_pubkey_root_file_sha256: evidence.profile.by_pubkey_root_file_sha256,
        profile_search_root_file_sha256: evidence.profile.search_root_file_sha256,
        profile_follow_distance_seal_sha256: evidence.profile.follow_distance_seal_sha256,
        profile_distance_provenance: profile_rank_authority.clone(),
    })
}

fn seal_bytes_and_sha(seal: &TrancheSeal) -> Result<(Vec<u8>, String)> {
    let bytes = json_line(seal, "v3 tranche seal")?;
    let sha256 = bytes_sha256(&bytes);
    Ok((bytes, sha256))
}

fn seal_path(seals_dir: &Path, generation: u64, sha256: &str) -> PathBuf {
    seals_dir.join(format!("{generation:020}-{sha256}.json"))
}

fn persist_seal(seals_dir: &Path, seal: &TrancheSeal) -> Result<String> {
    let (bytes, sha256) = seal_bytes_and_sha(seal)?;
    let path = seal_path(seals_dir, seal.generation, &sha256);
    persist_immutable_bytes(&path, &bytes, "v3 tranche seal")?;
    Ok(sha256)
}

fn load_seal(seals_dir: &Path, generation: u64, sha256: &str) -> Result<TrancheSeal> {
    validate_sha256("tranche seal SHA-256", sha256)?;
    let path = seal_path(seals_dir, generation, sha256);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    require_sha256("tranche seal SHA-256", &bytes_sha256(&bytes), sha256)?;
    let seal: TrancheSeal =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if seal.version != TRANCHE_STATE_VERSION || seal.generation != generation {
        anyhow::bail!("tranche seal metadata differs from its state reference");
    }
    Ok(seal)
}

fn validate_candidate_pin(label: &str, candidate: &CandidatePin) -> Result<()> {
    let root =
        parse_root_text(&candidate.root).with_context(|| format!("parse {label} manifest root"))?;
    if cid_to_nhash(&root)? != candidate.root {
        anyhow::bail!("{label} manifest root is not canonical nhash text");
    }
    if candidate.built_roots.len() != NostrEventIndex::ALL.len() {
        anyhow::bail!("{label} does not contain exactly nine index roots");
    }
    for index in NostrEventIndex::ALL {
        let encoded = candidate
            .built_roots
            .get(&index.stable_id())
            .with_context(|| format!("{label} omitted {} root", index.name()))?;
        if encoded.is_empty() {
            anyhow::bail!("{label} has an empty {} root", index.name());
        }
        let root = parse_root_text(encoded)
            .with_context(|| format!("parse {label} {} root", index.name()))?;
        if cid_to_nhash(&root)? != *encoded {
            anyhow::bail!("{label} {} root is not canonical nhash text", index.name());
        }
    }
    Ok(())
}

fn validate_stage_prefix_schema(
    label: &str,
    prefix: &StagePrefixSeal,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<()> {
    for (digest_label, digest) in [
        ("segment chain SHA-256", &prefix.segment_chain_sha256),
        ("event CID chain SHA-256", &prefix.event_cid_chain_sha256),
        (
            "observed staging state SHA-256",
            &prefix.observed_stage_state_sha256,
        ),
    ] {
        validate_sha256(&format!("{label} {digest_label}"), digest)?;
    }
    if prefix.next_author > policy.author_count || prefix.events_selected > prefix.events_seen {
        anyhow::bail!("{label} has impossible cursor or event counters");
    }
    if (prefix.next_author == 0) != (prefix.segment_count == 0)
        || prefix.segment_count > prefix.next_author
    {
        anyhow::bail!("{label} has an impossible immutable segment count");
    }
    Ok(())
}

fn validate_profile_rank_authority(
    authority: &ProfileDistanceProvenance,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<()> {
    for (label, digest) in [
        (
            "profile rank-decisions file SHA-256",
            authority.rank_decisions_file_sha256.as_str(),
        ),
        (
            "profile rank-decisions report SHA-256",
            authority.rank_decisions_report_sha256.as_str(),
        ),
        (
            "profile rank-decisions semantic SHA-256",
            authority.rank_decisions_sha256.as_str(),
        ),
        (
            "profile rank social-graph SHA-256",
            authority.social_graph_sha256.as_str(),
        ),
        (
            "profile rank eligible-author SHA-256",
            authority.eligible_authors_sha256.as_str(),
        ),
    ] {
        validate_sha256(label, digest)?;
    }
    validate_lower_hex_32(
        "profile rank social-graph root",
        &authority.social_graph_root,
    )?;
    if authority.eligible_authors_sha256 != policy.author_allowlist_sha256
        || authority.eligible_count != policy.author_count
        || authority.record_count
            != authority
                .eligible_count
                .checked_add(authority.excluded_count)
                .context("profile rank authority count overflow")?
    {
        anyhow::bail!(
            "profile rank authority does not bind the exact full crawl allowlist and decision census"
        );
    }
    Ok(())
}

fn validate_state_schema(state: &BulkTrancheState) -> Result<()> {
    if state.version != TRANCHE_STATE_VERSION
        || state.btree_order < 2
        || state.btree_update_concurrency == 0
        || state.index_commit_batch_size == 0
        || state.ordered_allowlist_count != state.policy.author_count
        || state.ordered_allowlist_sha256 != state.policy.author_allowlist_sha256
    {
        anyhow::bail!("invalid v3 tranche state schema or pinned build configuration");
    }
    validate_sha256(
        "v3 ordered allowlist SHA-256",
        &state.ordered_allowlist_sha256,
    )?;
    for (label, value) in [
        (
            "v3 spool identity marker SHA-256",
            &state.spool_identity.marker_sha256,
        ),
        ("v3 spool identity", &state.spool_identity.id),
        ("v3 serving event SHA-256", &state.serving.event_sha256),
        ("v3 accepted audit SHA-256", &state.last_evidence.sha256),
        (
            "v3 accepted v2 state SHA-256",
            &state.last_evidence.v2_state_sha256,
        ),
        (
            "v3 accepted stage state SHA-256",
            &state.last_evidence.stage_state_sha256,
        ),
        (
            "v3 accepted PoolStore catalog SHA-256",
            &state.last_evidence.pool_catalog_sha256,
        ),
        (
            "v3 accepted profile-by-pubkey SHA-256",
            &state.last_evidence.profile_by_pubkey_root_file_sha256,
        ),
        (
            "v3 accepted profile-search SHA-256",
            &state.last_evidence.profile_search_root_file_sha256,
        ),
    ] {
        validate_sha256(label, value)?;
    }
    validate_lower_hex_32("v3 serving event id", &state.serving.event_id)?;
    validate_lower_hex_32("v3 serving publisher pubkey", &state.serving.event_pubkey)?;
    let serving_root = parse_root_text(&state.serving.root).context("parse v3 serving root pin")?;
    if cid_to_nhash(&serving_root)? != state.serving.root {
        anyhow::bail!("v3 serving root pin is not canonical nhash text");
    }
    validate_candidate_pin("v3 last validated candidate", &state.last_validated)?;
    if state.last_evidence.candidate_root != state.last_validated.root {
        anyhow::bail!("v3 accepted evidence candidate differs from last validated candidate");
    }
    validate_profile_rank_authority(&state.profile_rank_authority, &state.policy)?;
    if state.last_evidence.profile_distance_provenance != state.profile_rank_authority
        || state.last_evidence.trusted_full_author_count != state.policy.author_count
    {
        anyhow::bail!("v3 audit evidence and profile rank authority provenance disagree");
    }
    for (label, digest) in [
        (
            "v3 accepted policy SHA-256",
            state.last_evidence.trusted_policy_sha256.as_str(),
        ),
        (
            "v3 accepted PoolStore manifest SHA-256",
            state.last_evidence.pool_manifest_sha256.as_str(),
        ),
        (
            "v3 accepted profile follow-distance seal SHA-256",
            state
                .last_evidence
                .profile_follow_distance_seal_sha256
                .as_str(),
        ),
    ] {
        validate_sha256(label, digest)?;
    }
    validate_stage_prefix_schema(
        "v3 rolling prefix",
        &state.working.rolling_prefix,
        &state.policy,
    )?;
    if state.working.next_author != state.working.rolling_prefix.next_author
        || state.working.events_seen != state.working.rolling_prefix.events_seen
        || state.working.events_selected != state.working.rolling_prefix.events_selected
        || state.working.live_bytes_selected != state.working.rolling_prefix.live_bytes_selected
    {
        anyhow::bail!("v3 working counters differ from the durable rolling prefix");
    }
    if let Some(frozen_prefix) = state.working.frozen_prefix.as_ref() {
        validate_stage_prefix_schema("v3 frozen prefix", frozen_prefix, &state.policy)?;
        if !frozen_prefix.immutable_prefix_eq(&state.working.rolling_prefix) {
            anyhow::bail!("v3 frozen prefix differs from the durable rolling prefix");
        }
    }
    if let Some(profile_distances) = state.working.frozen_profile_distances.as_ref() {
        validate_sha256(
            "v3 frozen profile follow-distance seal SHA-256",
            &profile_distances.sha256,
        )?;
        if profile_distances.retained_profile_count > state.profile_rank_authority.eligible_count {
            anyhow::bail!("v3 frozen profile count exceeds the frozen eligible rank census");
        }
    }
    if (state.working.segment_event_offset == 0 && state.working.active_segment_sha256.is_some())
        || (state.working.segment_event_offset > 0 && state.working.active_segment_sha256.is_none())
    {
        anyhow::bail!("v3 partial segment offset and segment SHA-256 pin disagree");
    }
    if let Some(sha256) = state.working.active_segment_sha256.as_deref() {
        validate_sha256("v3 active staged segment SHA-256", sha256)?;
    }
    match state.phase {
        TranchePhase::Prepare => {
            if state.active_seal_sha256.is_some()
                || state.pending_seal_sha256.is_none()
                || state.working.frozen_prefix.is_none()
                || state.working.frozen_profile_distances.is_some()
            {
                anyhow::bail!("v3 Prepare state has invalid active/pending seal references");
            }
        }
        TranchePhase::Appending => {
            if state.active_seal_sha256.is_none()
                || state.pending_seal_sha256.is_some()
                || state.working.frozen_prefix.is_some()
                || state.working.frozen_profile_distances.is_some()
            {
                anyhow::bail!("v3 post-Prepare state has invalid active/pending seal references");
            }
        }
        _ => {
            if state.active_seal_sha256.is_none()
                || state.pending_seal_sha256.is_some()
                || state.working.frozen_prefix.is_none()
                || state.working.frozen_profile_distances.is_none()
            {
                anyhow::bail!("v3 frozen-or-later state has invalid seal references");
            }
        }
    }
    if let Some(sha256) = state.active_seal_sha256.as_deref() {
        validate_sha256("v3 active seal SHA-256", sha256)?;
    }
    if let Some(sha256) = state.pending_seal_sha256.as_deref() {
        validate_sha256("v3 pending seal SHA-256", sha256)?;
    }
    Ok(())
}

fn validate_active_seal(state: &BulkTrancheState, seal: &TrancheSeal) -> Result<()> {
    validate_stage_prefix_schema("active v3 seal prefix", &seal.prefix, &state.policy)?;
    let purpose_matches = match seal.purpose {
        TrancheSealPurpose::Prepare => {
            seal.internal_candidate.as_ref() == Some(&state.last_validated)
                && seal.evidence.as_ref() == Some(&state.last_evidence)
                && seal.frozen_profile_distances.is_none()
        }
        TrancheSealPurpose::Freeze => {
            seal.internal_candidate.is_none()
                && seal.evidence.is_none()
                && state
                    .working
                    .frozen_prefix
                    .as_ref()
                    .is_some_and(|prefix| prefix.immutable_prefix_eq(&seal.prefix))
        }
    };
    if seal.policy != state.policy
        || seal.spool_identity != state.spool_identity
        || seal.serving != state.serving
        || seal.ordered_allowlist_count != state.ordered_allowlist_count
        || seal.ordered_allowlist_sha256 != state.ordered_allowlist_sha256
        || state.ordered_allowlist_count != state.policy.author_count
        || state.ordered_allowlist_sha256 != state.policy.author_allowlist_sha256
        || !purpose_matches
        || seal.profile_rank_authority != state.profile_rank_authority
        || seal.frozen_profile_distances != state.working.frozen_profile_distances
        || seal.publication_intent != state.publication_intent
        || seal.publication_receipt != state.publication_receipt
    {
        anyhow::bail!("active v3 seal metadata differs from durable tranche state");
    }
    validate_sha256(
        "active v3 seal ordered allowlist SHA-256",
        &seal.ordered_allowlist_sha256,
    )?;
    Ok(())
}

fn load_state(path: &Path) -> Result<Option<(BulkTrancheState, Vec<u8>, String)>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let sha256 = bytes_sha256(&bytes);
            let state: BulkTrancheState = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse v3 state {}", path.display()))?;
            if state.version != TRANCHE_STATE_VERSION {
                anyhow::bail!("unsupported bulk tranche state version {}", state.version);
            }
            validate_state_schema(&state)?;
            Ok(Some((state, bytes, sha256)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub(crate) fn load_bulk_tranche_progress(data_dir: &Path) -> Result<Option<(usize, u64)>> {
    let (state_path, _, _, _, _) = tranche_paths(data_dir);
    let Some((state, _, _)) = load_state(&state_path)? else {
        return Ok(None);
    };
    Ok(Some((
        state.working.next_author,
        state.working.live_bytes_selected,
    )))
}

fn persist_state(path: &Path, state: &BulkTrancheState) -> Result<String> {
    persist_json_atomic(path, state, "v3 bulk tranche state")?;
    let bytes = std::fs::read(path)
        .with_context(|| format!("re-read persisted v3 state {}", path.display()))?;
    Ok(bytes_sha256(&bytes))
}

fn transition_output(
    state: &BulkTrancheState,
    state_sha256: String,
) -> BulkTrancheTransitionOutput {
    BulkTrancheTransitionOutput {
        version: state.version,
        phase: serde_json::to_value(&state.phase)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_string()),
        generation: state.generation,
        state_sha256,
        active_seal_sha256: state.active_seal_sha256.clone(),
        next_author: state.working.next_author,
        authors_total: state.policy.author_count,
    }
}

fn write_output(output: &BulkTrancheTransitionOutput, out: Option<&Path>) -> Result<()> {
    match out {
        None => {
            println!("{}", serde_json::to_string_pretty(output)?);
            Ok(())
        }
        Some(path) if path == Path::new("-") => {
            println!("{}", serde_json::to_string_pretty(output)?);
            Ok(())
        }
        Some(path) => persist_json_atomic(path, output, "v3 tranche transition output"),
    }
}

pub(crate) fn prepare_bulk_tranche(
    data_dir: &Path,
    options: BulkTranchePrepareOptions,
) -> Result<BulkTrancheTransitionOutput> {
    if options.btree_order < 2
        || options.btree_update_concurrency == 0
        || options.index_commit_batch_size == 0
    {
        anyhow::bail!(
            "v3 prepare requires B-tree order >= 2 and non-zero update concurrency and commit batch size"
        );
    }
    let (state_path, seals_dir, evidence_dir, serving_events_dir, marker_path) =
        tranche_paths(data_dir);
    let existing_state = load_state(&state_path)?;
    let (v2_state_path, _) = bulk_paths(data_dir);
    let stage_state_path = options
        .staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_STATE_FILE);
    let v2_state_bytes = std::fs::read(&v2_state_path)
        .with_context(|| format!("read v2 state {}", v2_state_path.display()))?;
    let stage_state_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("read stage state {}", stage_state_path.display()))?;
    let v2_state_sha256 = bytes_sha256(&v2_state_bytes);
    let stage_state_sha256 = bytes_sha256(&stage_state_bytes);
    require_sha256(
        "v2 projection state SHA-256",
        &v2_state_sha256,
        &options.expected_v2_state_sha256,
    )?;
    if existing_state.is_none() {
        require_sha256(
            "staging state SHA-256",
            &stage_state_sha256,
            &options.expected_stage_state_sha256,
        )?;
    } else {
        validate_sha256(
            "pinned staging state SHA-256",
            &options.expected_stage_state_sha256,
        )?;
    }
    let v2_state: BulkProjectionState =
        serde_json::from_slice(&v2_state_bytes).context("parse terminal v2 projection state")?;
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&stage_state_bytes).context("parse current staging state")?;
    let candidate = validate_v2_terminal_state(&v2_state)?;
    validate_stage_covers_v2_terminal(&stage, &v2_state, existing_state.is_none())?;
    let authors = load_ordered_allowlist(&options.eligible_authors, &v2_state.policy)?;
    let ordered_allowlist_bytes = canonical_ordered_allowlist_bytes(&authors);
    require_sha256(
        "canonical ordered author allowlist SHA-256",
        &bytes_sha256(&ordered_allowlist_bytes),
        &v2_state.policy.author_allowlist_sha256,
    )?;
    let profile_rank_authority = load_pinned_profile_rank_decisions(
        &options.profile_rank_decisions_file,
        &options.expected_profile_rank_decisions_file_sha256,
        &options.profile_rank_decisions_report,
        &options.expected_profile_rank_decisions_report_sha256,
    )?;
    require_profile_rank_policy_binding(
        Some(&profile_rank_authority),
        &v2_state.policy.author_allowlist_sha256,
        v2_state.policy.author_count,
    )?;
    validate_profile_rank_authority(&profile_rank_authority.evidence, &v2_state.policy)?;
    // This one-time locked Prepare migration gives pre-claim staged history
    // the same immutable O(1) boundary index as every newly fetched segment.
    // Subsequent append invocations never scan or backfill the catalog.
    backfill_stage_segment_claims(&options.staging_data_dir, &v2_state.policy)?;
    let prefix = attest_stage_prefix(
        &options.staging_data_dir,
        StagePrefixTarget {
            boundary: v2_state.next_author,
            durable_next_author: stage.next_author,
            events_seen: v2_state.events_seen,
            events_selected: v2_state.events_selected,
            live_bytes_selected: v2_state.live_bytes_selected,
        },
        options.expected_stage_state_sha256.clone(),
        &v2_state.policy,
    )?;
    let (serving, serving_event_bytes) = serving_root_pin(&options)?;

    let audit_bytes = std::fs::read(&options.audit_evidence)
        .with_context(|| format!("read audit evidence {}", options.audit_evidence.display()))?;
    let audit_sha256 = bytes_sha256(&audit_bytes);
    let evidence = accept_audit_evidence(
        &audit_bytes,
        audit_sha256.clone(),
        &candidate,
        &v2_state_sha256,
        &options.expected_stage_state_sha256,
        &v2_state,
        &profile_rank_authority.evidence,
    )?;

    let (spool_identity, spool_identity_bytes) = spool_identity(data_dir)?;
    let evidence_path = evidence_dir.join(format!("{audit_sha256}.json"));
    let ordered_allowlist_path =
        ordered_allowlist_evidence_path(&evidence_dir, &v2_state.policy.author_allowlist_sha256);
    let (rank_decisions_path, rank_report_path) =
        profile_rank_evidence_paths(&evidence_dir, &profile_rank_authority.evidence);
    let serving_event_path = serving_events_dir.join(format!("{}.json", serving.event_id));

    let seal = TrancheSeal {
        version: TRANCHE_STATE_VERSION,
        generation: 0,
        parent_seal_sha256: None,
        purpose: TrancheSealPurpose::Prepare,
        policy: v2_state.policy.clone(),
        ordered_allowlist_sha256: v2_state.policy.author_allowlist_sha256.clone(),
        ordered_allowlist_count: authors.len(),
        prefix: prefix.clone(),
        spool_identity: spool_identity.clone(),
        internal_candidate: Some(candidate.clone()),
        evidence: Some(evidence.clone()),
        profile_rank_authority: profile_rank_authority.evidence.clone(),
        frozen_profile_distances: None,
        serving: serving.clone(),
        publication_intent: None,
        publication_receipt: None,
    };
    let (_, seal_sha256) = seal_bytes_and_sha(&seal)?;
    let prepare_state = BulkTrancheState {
        version: TRANCHE_STATE_VERSION,
        phase: TranchePhase::Prepare,
        generation: 0,
        policy: v2_state.policy.clone(),
        ordered_allowlist_sha256: v2_state.policy.author_allowlist_sha256.clone(),
        ordered_allowlist_count: v2_state.policy.author_count,
        active_seal_sha256: None,
        pending_seal_sha256: Some(seal_sha256.clone()),
        serving: serving.clone(),
        last_validated: candidate.clone(),
        last_evidence: evidence.clone(),
        profile_rank_authority: profile_rank_authority.evidence.clone(),
        spool_identity: spool_identity.clone(),
        btree_order: options.btree_order,
        btree_update_concurrency: options.btree_update_concurrency,
        index_commit_batch_size: options.index_commit_batch_size,
        working: WorkingProjection {
            next_author: v2_state.next_author,
            segment_event_offset: 0,
            active_segment_sha256: None,
            events_seen: v2_state.events_seen,
            events_selected: v2_state.events_selected,
            live_bytes_selected: v2_state.live_bytes_selected,
            rolling_prefix: prefix.clone(),
            frozen_profile_distances: None,
            built_roots: v2_state.built_roots.clone(),
            candidate_root: v2_state.complete_root.clone(),
            frozen_prefix: Some(prefix),
        },
        publication_intent: None,
        publication_receipt: None,
    };
    let mut existing_appending = None;
    if let Some((existing, _, state_sha256)) = existing_state {
        if existing.phase == TranchePhase::Appending {
            let mut expected = prepare_state.clone();
            expected.phase = TranchePhase::Appending;
            expected.active_seal_sha256 = Some(seal_sha256.clone());
            expected.pending_seal_sha256 = None;
            expected.working.built_roots.clear();
            expected.working.candidate_root = None;
            expected.working.frozen_prefix = None;
            if existing != expected {
                anyhow::bail!("existing v3 Appending state differs from exact prepare inputs");
            }
            existing_appending = Some((existing, state_sha256));
        } else if existing != prepare_state {
            anyhow::bail!("existing v3 state differs from exact resumable Prepare state");
        }
    } else {
        recheck_trusted_profile_rank_decisions(Some(&profile_rank_authority))?;
        persist_state(&state_path, &prepare_state)?;
    }

    // Prepare is the first mutable v3 write. A crash from this point retains
    // the old complete roots in both v2 and Prepare state, while an exact
    // rerun can adopt or recreate every immutable dependency below.
    persist_immutable_bytes(&marker_path, &spool_identity_bytes, "spool identity marker")?;
    persist_immutable_bytes(
        &hashtree_cli::socialgraph::profile_publication_fence_path(data_dir),
        PROFILE_PUBLICATION_FENCE_BYTES,
        "profile publication fence",
    )?;
    persist_immutable_bytes(&evidence_path, &audit_bytes, "accepted audit evidence")?;
    persist_immutable_bytes(
        &ordered_allowlist_path,
        &ordered_allowlist_bytes,
        "canonical ordered author allowlist evidence",
    )?;
    persist_immutable_bytes(
        &rank_decisions_path,
        &profile_rank_authority.decisions_bytes,
        "profile rank-decisions evidence",
    )?;
    persist_immutable_bytes(
        &rank_report_path,
        &profile_rank_authority.report_bytes,
        "profile rank-decisions report evidence",
    )?;
    persist_immutable_bytes(
        &serving_event_path,
        &serving_event_bytes,
        "externally resolved serving event",
    )?;
    let persisted_seal_sha256 = persist_seal(&seals_dir, &seal)?;
    if persisted_seal_sha256 != seal_sha256 {
        anyhow::bail!("persisted prepare seal SHA-256 changed unexpectedly");
    }
    let copied_rank_authority =
        load_copied_profile_rank_authority(&evidence_dir, &profile_rank_authority.evidence)?;
    recheck_trusted_profile_rank_decisions(Some(&copied_rank_authority))?;
    if let Some((existing, state_sha256)) = existing_appending {
        let output = transition_output(&existing, state_sha256);
        write_output(&output, options.out.as_deref())?;
        return Ok(output);
    }
    let mut appending = prepare_state;
    appending.phase = TranchePhase::Appending;
    appending.active_seal_sha256 = Some(seal_sha256);
    appending.pending_seal_sha256 = None;
    appending.working.built_roots.clear();
    appending.working.candidate_root = None;
    appending.working.frozen_prefix = None;
    let state_sha256 = persist_state(&state_path, &appending)?;
    let output = transition_output(&appending, state_sha256);
    write_output(&output, options.out.as_deref())?;
    Ok(output)
}

pub(crate) async fn append_bulk_tranche(
    stores: ProjectionStores<'_>,
    data_dir: &Path,
    options: BulkTrancheAppendOptions,
) -> Result<BulkTrancheTransitionOutput> {
    let (state_path, seals_dir, evidence_dir, _, marker_path) = tranche_paths(data_dir);
    let (mut state, _, state_sha256) =
        load_state(&state_path)?.context("v3 tranche state does not exist; run prepare first")?;
    require_sha256(
        "v3 tranche state SHA-256",
        &state_sha256,
        &options.expected_state_sha256,
    )?;
    if state.phase != TranchePhase::Appending {
        anyhow::bail!("v3 append requires Appending phase");
    }
    if options.max_segments == 0 {
        anyhow::bail!("v3 append max-segments must be non-zero");
    }
    if !state.working.built_roots.is_empty()
        || state.working.candidate_root.is_some()
        || state.working.frozen_prefix.is_some()
    {
        anyhow::bail!("v3 Appending state contains frozen build or candidate data");
    }
    validate_spool_identity(data_dir, &marker_path, &state.spool_identity)?;
    validate_profile_publication_fence(data_dir)?;
    let profile_rank_authority =
        load_copied_profile_rank_authority(&evidence_dir, &state.profile_rank_authority)?;
    let active_sha = state
        .active_seal_sha256
        .as_deref()
        .context("v3 Appending state has no active seal")?;
    let active = load_seal(&seals_dir, state.generation, active_sha)?;
    if active.purpose != TrancheSealPurpose::Prepare {
        anyhow::bail!("v3 Appending state does not reference a Prepare seal");
    }
    validate_active_seal(&state, &active)?;
    let current_stage_bytes = std::fs::read(
        options
            .staging_data_dir
            .join(STAGE_DIR)
            .join(STAGE_STATE_FILE),
    )
    .context("read current staging state")?;
    let current_stage_sha256 = bytes_sha256(&current_stage_bytes);
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&current_stage_bytes).context("parse current staging state")?;
    validate_stage_state(&stage, &state.policy, state.ordered_allowlist_count)?;
    if stage.next_author < state.working.next_author {
        anyhow::bail!(
            "staging cursor {} is behind durable v3 working cursor {}",
            stage.next_author,
            state.working.next_author
        );
    }
    if state.working.next_author < active.prefix.next_author {
        anyhow::bail!("v3 working cursor precedes its active sealed prefix");
    }
    if state.working.next_author == active.prefix.next_author
        && !state
            .working
            .rolling_prefix
            .immutable_prefix_eq(&active.prefix)
    {
        anyhow::bail!("v3 rolling prefix differs from its active Prepare seal");
    }

    let (_, spool_path) = bulk_paths(data_dir);
    let spool = BulkProjectionSpool::open(&spool_path)?;
    let event_store_options = NostrEventStoreOptions {
        btree_order: Some(state.btree_order),
        btree_update_concurrency: Some(state.btree_update_concurrency),
        index_commit_batch_size: Some(state.index_commit_batch_size),
    };
    let target_event_store =
        NostrEventStore::with_options(stores.durable.store_arc(), event_store_options.clone());
    let staging_event_store =
        NostrEventStore::with_options(stores.staging.store_arc(), event_store_options);

    let mut completed_segments = 0usize;
    let mut persisted_state_sha256 = state_sha256;
    while completed_segments < options.max_segments && state.working.next_author < stage.next_author
    {
        let (segment_path, segment_bytes, segment) = load_stage_segment_with_bytes(
            &options.staging_data_dir,
            state.working.next_author,
            &state.policy,
        )
        .with_context(|| {
            format!(
                "load immutable staged segment beginning at v3 cursor {}",
                state.working.next_author
            )
        })?;
        if segment.end_author > stage.next_author {
            anyhow::bail!(
                "staged segment {}..{} is not covered by durable staging cursor {}",
                segment.start_author,
                segment.end_author,
                stage.next_author
            );
        }
        let segment_sha256 = bytes_sha256(&segment_bytes);
        if state.working.segment_event_offset == 0 {
            if state.working.active_segment_sha256.is_some() {
                anyhow::bail!("v3 working state pins a segment while its event offset is zero");
            }
        } else {
            let pinned = state
                .working
                .active_segment_sha256
                .as_deref()
                .context("v3 partial segment cursor has no pinned segment SHA-256")?;
            require_sha256("active staged segment SHA-256", &segment_sha256, pinned)?;
        }
        if state.working.segment_event_offset > segment.event_cids.len() {
            anyhow::bail!("v3 working offset exceeds immutable staged segment length");
        }
        loop {
            let started = std::time::Instant::now();
            let event_end = state
                .working
                .segment_event_offset
                .checked_add(state.index_commit_batch_size)
                .context("v3 segment event offset overflow")?
                .min(segment.event_cids.len());
            let cids = segment.event_cids[state.working.segment_event_offset..event_end]
                .iter()
                .map(|root| parse_root_text(root))
                .collect::<Result<Vec<_>>>()?;
            let replay = replay_staged_event_chunk(
                stores,
                &spool,
                &target_event_store,
                &staging_event_store,
                cids,
                ProfileDistanceAuthority::FrozenRankDecisions(&profile_rank_authority.decisions),
            )
            .await?;
            let completed_segment = event_end == segment.event_cids.len();
            if completed_segment {
                extend_stage_prefix(
                    &mut state.working.rolling_prefix,
                    &segment_path,
                    &segment_bytes,
                    &segment,
                    &current_stage_sha256,
                )?;
                state.working.next_author = state.working.rolling_prefix.next_author;
                state.working.segment_event_offset = 0;
                state.working.active_segment_sha256 = None;
                state.working.events_seen = state.working.rolling_prefix.events_seen;
                state.working.events_selected = state.working.rolling_prefix.events_selected;
                state.working.live_bytes_selected =
                    state.working.rolling_prefix.live_bytes_selected;
                completed_segments = completed_segments
                    .checked_add(1)
                    .context("v3 completed segment count overflow")?;
            } else {
                state.working.segment_event_offset = event_end;
                state.working.active_segment_sha256 = Some(segment_sha256.clone());
            }
            persisted_state_sha256 = persist_state(&state_path, &state)?;
            eprintln!(
                "Nostr v3 tranche append checkpoint: authors={}/{} staged_authors={} segment_event_offset={}/{} retained={} replaced={} skipped={} index_entries={} reused_records={} spool_missing_candidates={} durable_reused_candidates={} stored_candidates={} reused_exact_batch={} completed_segment={} completed_segments={} stage_load_ms={} replay_plan_ms={} durable_probe_ms={} target_store_ms={} target_sync_ms={} spool_write_ms={} spool_sync_ms={} profile_sync_ms={} batch_elapsed_ms={}",
                state.working.next_author,
                state.ordered_allowlist_count,
                stage.next_author,
                state.working.segment_event_offset,
                segment.event_cids.len(),
                replay.apply.inserted,
                replay.apply.replaced,
                replay.apply.skipped,
                replay.apply.index_entries,
                replay.apply.reused_records,
                replay.spool_missing_candidates,
                replay.apply.durable_reused_candidates,
                replay.apply.stored_candidates,
                replay.apply.reused_exact_batch,
                completed_segment,
                completed_segments,
                replay.stage_load_ms,
                replay.replay_plan_ms,
                replay.durable_probe_ms,
                replay.target_store_ms,
                replay.target_sync_ms,
                replay.apply.spool_write_ms,
                replay.apply.spool_sync_ms,
                replay.profile_sync_ms,
                started.elapsed().as_millis()
            );
            if completed_segment {
                break;
            }
        }
    }
    recheck_trusted_profile_rank_decisions(Some(&profile_rank_authority))?;
    let output = transition_output(&state, persisted_state_sha256);
    write_output(&output, options.out.as_deref())?;
    Ok(output)
}

pub(crate) fn freeze_bulk_tranche(
    data_dir: &Path,
    options: BulkTrancheFreezeOptions,
) -> Result<BulkTrancheTransitionOutput> {
    let (state_path, seals_dir, evidence_dir, _, marker_path) = tranche_paths(data_dir);
    let (mut state, _, state_sha256) =
        load_state(&state_path)?.context("v3 tranche state does not exist; run prepare first")?;
    require_sha256(
        "v3 tranche state SHA-256",
        &state_sha256,
        &options.expected_state_sha256,
    )?;
    if state.phase != TranchePhase::Appending {
        anyhow::bail!("v3 freeze requires Appending phase");
    }
    if state.working.segment_event_offset != 0 {
        anyhow::bail!("v3 freeze cannot split a staged segment");
    }
    if state.working.active_segment_sha256.is_some() {
        anyhow::bail!("v3 freeze found a pinned partial segment at a zero event offset");
    }
    if options.through_author != state.working.next_author {
        anyhow::bail!(
            "v3 freeze boundary {} differs from durable working boundary {}",
            options.through_author,
            state.working.next_author
        );
    }
    validate_spool_identity(data_dir, &marker_path, &state.spool_identity)?;
    validate_profile_publication_fence(data_dir)?;
    let profile_rank_authority =
        load_copied_profile_rank_authority(&evidence_dir, &state.profile_rank_authority)?;
    let parent_sha = state
        .active_seal_sha256
        .clone()
        .context("v3 Appending state has no active seal")?;
    let parent = load_seal(&seals_dir, state.generation, &parent_sha)?;
    if parent.purpose != TrancheSealPurpose::Prepare {
        anyhow::bail!("v3 freeze parent is not an Appending Prepare seal");
    }
    validate_active_seal(&state, &parent)?;
    let stage_state_path = options
        .staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_STATE_FILE);
    let stage_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("read {}", stage_state_path.display()))?;
    let stage_sha256 = bytes_sha256(&stage_bytes);
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&stage_bytes).context("parse current staging state")?;
    validate_stage_state(&stage, &state.policy, state.ordered_allowlist_count)?;
    if stage.next_author < state.working.next_author {
        anyhow::bail!("current staging state cannot cover the requested v3 freeze boundary");
    }
    let frozen_prefix = attest_stage_prefix(
        &options.staging_data_dir,
        StagePrefixTarget {
            boundary: state.working.next_author,
            durable_next_author: stage.next_author,
            events_seen: state.working.events_seen,
            events_selected: state.working.events_selected,
            live_bytes_selected: state.working.live_bytes_selected,
        },
        stage_sha256,
        &state.policy,
    )?;
    if !frozen_prefix.immutable_prefix_eq(&state.working.rolling_prefix) {
        anyhow::bail!("full staged prefix reattestation differs from the rolling append seal");
    }
    let (_, spool_path) = bulk_paths(data_dir);
    let spool = BulkProjectionSpool::open(&spool_path)?;
    let (profile_distance_sha256, retained_profile_count) = spool
        .profile_distance_seal_for_frozen_authority(&profile_rank_authority.decisions)
        .context("seal frozen profile rank decisions against retained metadata winners")?;
    let frozen_profile_distances = FrozenProfileDistanceSeal {
        sha256: profile_distance_sha256,
        retained_profile_count,
    };
    recheck_trusted_profile_rank_decisions(Some(&profile_rank_authority))?;
    let generation = state
        .generation
        .checked_add(1)
        .context("tranche generation overflow")?;
    let seal = TrancheSeal {
        version: TRANCHE_STATE_VERSION,
        generation,
        parent_seal_sha256: Some(parent_sha),
        purpose: TrancheSealPurpose::Freeze,
        policy: state.policy.clone(),
        ordered_allowlist_sha256: parent.ordered_allowlist_sha256,
        ordered_allowlist_count: parent.ordered_allowlist_count,
        prefix: frozen_prefix.clone(),
        spool_identity: state.spool_identity.clone(),
        internal_candidate: None,
        evidence: None,
        profile_rank_authority: state.profile_rank_authority.clone(),
        frozen_profile_distances: Some(frozen_profile_distances.clone()),
        serving: state.serving.clone(),
        publication_intent: None,
        publication_receipt: None,
    };
    let seal_sha256 = persist_seal(&seals_dir, &seal)?;
    state.phase = TranchePhase::Freeze;
    state.generation = generation;
    state.active_seal_sha256 = Some(seal_sha256);
    state.pending_seal_sha256 = None;
    state.working.frozen_prefix = Some(frozen_prefix);
    state.working.frozen_profile_distances = Some(frozen_profile_distances);
    state.working.built_roots.clear();
    state.working.candidate_root = None;
    let state_sha256 = persist_state(&state_path, &state)?;
    let output = transition_output(&state, state_sha256);
    write_output(&output, options.out.as_deref())?;
    Ok(output)
}

#[cfg(test)]
mod tests;
