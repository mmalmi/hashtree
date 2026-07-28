use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hashtree_cli::socialgraph::{
    profile_index_root_file_sha256, profile_repair_completion_path,
    profile_repair_completion_witness_bytes, PreparedProfileIndexRepair, ProfileIndexRoots,
    SocialGraphStore, PROFILE_REPAIR_FORMAT, PROFILE_REPAIR_RECEIPT_FORMAT,
};
use hashtree_cli::storage::{
    ProfileRepairRetentionLease, PROFILE_REPAIR_RETENTION_LEASE_FORMAT,
    PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH,
};
use hashtree_cli::HashtreeStore;
use hashtree_lmdb::{ReadOnlyPoolCatalogAudit, ReadOnlyPoolStore, SHARED_BLOB_POOL_DIR_NAME};
use nostr::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::{
    cid_to_nhash, parse_root_text, persist_immutable_bytes, stage_bytes_sha256,
    StagedNostrCrawlState, STAGE_DIR, STAGE_STATE_FILE,
};
use super::audit::{
    audit_profile_indexes_at_roots, load_pinned_profile_rank_decisions,
    recheck_trusted_profile_rank_decisions, require_profile_rank_policy_binding,
    validate_exact_event_index_parity_evidence, BulkProjectionExactIndexParityEvidence,
    BulkProjectionProfileAudit, ProfileDistanceProvenance,
};
use super::tranche::PROFILE_PUBLICATION_FENCE_BYTES;
use super::{
    bulk_paths, validate_terminal_stage_state, BulkProjectionSpool, BulkProjectionState,
    BULK_PROJECTION_VERSION,
};

const EVENT_BLOB_REPAIR_RECEIPT_FORMAT: &str =
    "nostr-index/bulk-projection-v2/event-blob-repair-v1/receipt";

#[cfg(test)]
type ProfileRepairBoundaryProbe = Arc<dyn Fn(&'static str) -> Result<()> + Send + Sync + 'static>;

#[cfg(test)]
fn profile_repair_boundary_probe() -> &'static std::sync::Mutex<Option<ProfileRepairBoundaryProbe>>
{
    static PROBE: std::sync::OnceLock<std::sync::Mutex<Option<ProfileRepairBoundaryProbe>>> =
        std::sync::OnceLock::new();
    PROBE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct ProfileRepairBoundaryProbeGuard;

#[cfg(test)]
impl Drop for ProfileRepairBoundaryProbeGuard {
    fn drop(&mut self) {
        if let Ok(mut probe) = profile_repair_boundary_probe().lock() {
            *probe = None;
        }
    }
}

#[cfg(test)]
fn install_profile_repair_boundary_probe(
    probe: ProfileRepairBoundaryProbe,
) -> ProfileRepairBoundaryProbeGuard {
    *profile_repair_boundary_probe()
        .lock()
        .expect("profile repair boundary probe lock poisoned") = Some(probe);
    ProfileRepairBoundaryProbeGuard
}

#[cfg(test)]
fn run_profile_repair_boundary_probe(boundary: &'static str) -> Result<()> {
    let probe = profile_repair_boundary_probe()
        .lock()
        .map_err(|_| anyhow::anyhow!("profile repair boundary probe lock poisoned"))?
        .clone();
    if let Some(probe) = probe {
        probe(boundary)?;
    }
    Ok(())
}

#[cfg(not(test))]
fn run_profile_repair_boundary_probe(_boundary: &'static str) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BulkProfileRepairOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: String,
    pub(crate) expected_stage_state_sha256: String,
    pub(crate) expected_policy_sha256: String,
    pub(crate) expected_spool_data_sha256: String,
    pub(crate) event_blob_repair_receipt: PathBuf,
    pub(crate) expected_event_blob_repair_receipt_sha256: String,
    pub(crate) profile_rank_decisions_file: PathBuf,
    pub(crate) expected_profile_rank_decisions_file_sha256: String,
    pub(crate) profile_rank_decisions_report: PathBuf,
    pub(crate) expected_profile_rank_decisions_report_sha256: String,
    pub(crate) expected_replayed_author_count: usize,
    pub(crate) expected_full_author_count: usize,
    pub(crate) expected_profiles_by_pubkey_root_file_sha256: String,
    pub(crate) expected_profile_search_root_file_sha256: String,
    pub(crate) required_profile_pubkeys: Vec<String>,
    pub(crate) btree_order: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RootPairPin {
    by_pubkey: String,
    by_pubkey_file_sha256: String,
    search: String,
    search_file_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct PoolCatalogPin {
    pub(super) stored_locations: u64,
    pub(super) sha256: String,
    pub(super) manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TrustedEventBlobRepairReceipt {
    format: String,
    intent_sha256: String,
    recovered_records: u64,
    missing_set_sha256: String,
    completion_pool_catalog: PoolCatalogPin,
    event_index_parity: BulkProjectionExactIndexParityEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProfileRepairIntent {
    format: String,
    data_dir: String,
    staging_data_dir: String,
    state_sha256: String,
    stage_state_sha256: String,
    policy_sha256: String,
    spool_data_sha256: String,
    profile_rank_decisions_file_sha256: String,
    profile_rank_decisions_report_sha256: String,
    profile_rank_provenance: ProfileDistanceProvenance,
    replayed_author_count: usize,
    full_author_count: usize,
    btree_order: usize,
    built_roots: BTreeMap<u8, String>,
    profile_records: usize,
    profile_records_sha256: String,
    required_profile_pubkeys: Vec<String>,
    retention_lease_sha256: String,
    prepublish_pool_catalog: PoolCatalogPin,
    event_index_parity: BulkProjectionExactIndexParityEvidence,
    old_roots: RootPairPin,
    new_roots: RootPairPin,
    prepublish_audit: BulkProjectionProfileAudit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProfileRepairReceipt {
    format: String,
    intent_sha256: String,
    state_sha256: String,
    stage_state_sha256: String,
    spool_data_sha256: String,
    profile_records: usize,
    profile_records_sha256: String,
    required_profile_pubkeys: Vec<String>,
    retention_lease_sha256: String,
    completion_pool_catalog: PoolCatalogPin,
    event_index_parity: BulkProjectionExactIndexParityEvidence,
    installed_roots: RootPairPin,
    postpublish_audit: BulkProjectionProfileAudit,
}

fn repair_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    hashtree_cli::socialgraph::profile_repair_evidence_paths(data_dir)
}

pub(super) fn require_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

pub(super) fn hash_file(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect pinned file {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "pinned path is not a direct regular file: {}",
            path.display()
        );
    }
    let file = File::open(path).with_context(|| format!("open pinned file {}", path.display()))?;
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);
    let mut buffer = vec![0u8; 8 * 1024 * 1024];
    let mut digest = Sha256::new();
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("hash pinned file {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

pub(super) fn open_exact_durable_pool(data_dir: &Path) -> Result<Arc<ReadOnlyPoolStore>> {
    let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
    let store = Arc::new(
        ReadOnlyPoolStore::open(&pool_path)
            .with_context(|| format!("open exact read-only PoolStore {}", pool_path.display()))?,
    );
    store
        .require_durable_external_blob_writes()
        .context("require durable external-blob writes for every Pool member")?;
    Ok(store)
}

pub(super) fn pin_committed_pool_catalog(store: &ReadOnlyPoolStore) -> Result<PoolCatalogPin> {
    let ReadOnlyPoolCatalogAudit {
        stored_locations,
        sha256,
        manifest_sha256,
    } = store
        .validate_committed_catalog()
        .context("validate exact fully committed PoolStore catalog")?;
    Ok(PoolCatalogPin {
        stored_locations,
        sha256,
        manifest_sha256,
    })
}

pub(super) fn validate_pool_catalog_pin(label: &str, pin: &PoolCatalogPin) -> Result<()> {
    if pin.stored_locations == 0 {
        anyhow::bail!("{label} contains no committed Pool locations");
    }
    require_sha256(&format!("{label} catalog SHA-256"), &pin.sha256)?;
    require_sha256(&format!("{label} manifest SHA-256"), &pin.manifest_sha256)
}

pub(super) fn canonical_json_bytes<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).with_context(|| format!("encode {label}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn load_trusted_event_blob_repair_receipt(
    path: &Path,
    expected_sha256: &str,
) -> Result<TrustedEventBlobRepairReceipt> {
    require_sha256("event-blob repair receipt SHA-256", expected_sha256)?;
    if !path.is_absolute() {
        anyhow::bail!("event-blob repair receipt path must be absolute");
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect event-blob repair receipt {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "event-blob repair receipt is not a direct regular file: {}",
            path.display()
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read event-blob repair receipt {}", path.display()))?;
    if stage_bytes_sha256(&bytes) != expected_sha256 {
        anyhow::bail!("event-blob repair receipt SHA-256 differs from the exact pin");
    }
    let receipt: TrustedEventBlobRepairReceipt =
        serde_json::from_slice(&bytes).context("decode exact event-blob repair receipt")?;
    if canonical_json_bytes(&receipt, "event-blob repair receipt")? != bytes {
        anyhow::bail!("event-blob repair receipt is not canonical JSON");
    }
    if receipt.format != EVENT_BLOB_REPAIR_RECEIPT_FORMAT {
        anyhow::bail!("event-blob repair receipt has the wrong format");
    }
    require_sha256("event-blob repair intent SHA-256", &receipt.intent_sha256)?;
    require_sha256(
        "event-blob repair missing-set SHA-256",
        &receipt.missing_set_sha256,
    )?;
    validate_pool_catalog_pin(
        "event-blob repair completion Pool snapshot",
        &receipt.completion_pool_catalog,
    )?;
    Ok(receipt)
}

fn validate_trusted_event_blob_repair_evidence(
    receipt: &TrustedEventBlobRepairReceipt,
    built_roots: &BTreeMap<u8, String>,
    btree_order: usize,
) -> Result<()> {
    validate_exact_event_index_parity_evidence(
        &receipt.event_index_parity,
        built_roots,
        btree_order,
    )
    .context("validate exact event-root parity from the pinned event-blob repair receipt")
}

fn profile_repair_retention_roots(
    built_roots: &BTreeMap<u8, String>,
    new_roots: &ProfileIndexRoots,
) -> Result<BTreeMap<String, String>> {
    let mut roots = BTreeMap::new();
    for (stable_id, encoded) in built_roots {
        let root = parse_root_text(encoded)
            .with_context(|| format!("parse retained event-index root {stable_id}"))?;
        roots.insert(format!("event-index-{stable_id}"), root.to_string());
    }
    for (label, root) in [
        (
            "profiles-by-pubkey",
            new_roots
                .by_pubkey
                .as_ref()
                .context("profile repair retention lease requires a profiles-by-pubkey root")?,
        ),
        (
            "profile-search",
            new_roots
                .search
                .as_ref()
                .context("profile repair retention lease requires a profile-search root")?,
        ),
    ] {
        roots.insert(label.to_string(), root.to_string());
    }
    Ok(roots)
}

#[allow(clippy::too_many_arguments)]
fn build_profile_repair_retention_lease(
    data_dir: &Path,
    staging_data_dir: &Path,
    state_sha256: &str,
    stage_state_sha256: &str,
    policy_sha256: &str,
    spool_data_sha256: &str,
    options: &BulkProfileRepairOptions,
    profile_records: usize,
    profile_records_sha256: &str,
    required_profile_pubkeys: &[String],
    built_roots: &BTreeMap<u8, String>,
    new_roots: &ProfileIndexRoots,
) -> Result<ProfileRepairRetentionLease> {
    let authority = BTreeMap::from([
        (
            "data_dir",
            data_dir
                .canonicalize()
                .context("canonicalize retention-lease data directory")?
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "staging_data_dir",
            staging_data_dir
                .canonicalize()
                .context("canonicalize retention-lease staging directory")?
                .to_string_lossy()
                .into_owned(),
        ),
        ("state_sha256", state_sha256.to_string()),
        ("stage_state_sha256", stage_state_sha256.to_string()),
        ("policy_sha256", policy_sha256.to_string()),
        ("spool_data_sha256", spool_data_sha256.to_string()),
        (
            "profile_rank_decisions_file_sha256",
            options.expected_profile_rank_decisions_file_sha256.clone(),
        ),
        (
            "profile_rank_decisions_report_sha256",
            options
                .expected_profile_rank_decisions_report_sha256
                .clone(),
        ),
        (
            "replayed_author_count",
            options.expected_replayed_author_count.to_string(),
        ),
        (
            "full_author_count",
            options.expected_full_author_count.to_string(),
        ),
        ("btree_order", options.btree_order.to_string()),
        ("profile_records", profile_records.to_string()),
        ("profile_records_sha256", profile_records_sha256.to_string()),
        (
            "required_profile_pubkeys",
            required_profile_pubkeys.join(","),
        ),
    ]);
    let authority_sha256 = stage_bytes_sha256(&canonical_json_bytes(
        &authority,
        "retention lease authority",
    )?);
    let lease = ProfileRepairRetentionLease {
        format: PROFILE_REPAIR_RETENTION_LEASE_FORMAT.to_string(),
        authority_sha256,
        roots: profile_repair_retention_roots(built_roots, new_roots)?,
    };
    lease.validate()?;
    Ok(lease)
}

fn load_profile_repair_retention_lease(
    data_dir: &Path,
    expected_sha256: &str,
    built_roots: &BTreeMap<u8, String>,
    new_roots: &ProfileIndexRoots,
) -> Result<ProfileRepairRetentionLease> {
    require_sha256("profile repair retention lease SHA-256", expected_sha256)?;
    let path = data_dir.join(PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("inspect repair retention lease {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        anyhow::bail!(
            "profile repair retention lease is not a direct regular file: {}",
            path.display()
        );
    }
    if metadata.len() > 64 * 1024 {
        anyhow::bail!(
            "profile repair retention lease exceeds 65536 bytes: {}",
            path.display()
        );
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read repair retention lease {}", path.display()))?;
    let actual_sha256 = stage_bytes_sha256(&bytes);
    if actual_sha256 != expected_sha256 {
        anyhow::bail!(
            "profile repair retention lease SHA-256 mismatch: expected {expected_sha256}, found {actual_sha256}"
        );
    }
    let lease: ProfileRepairRetentionLease = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode repair retention lease {}", path.display()))?;
    if lease.canonical_bytes()? != bytes {
        anyhow::bail!(
            "profile repair retention lease is not canonical: {}",
            path.display()
        );
    }
    if lease.roots != profile_repair_retention_roots(built_roots, new_roots)? {
        anyhow::bail!(
            "profile repair retention lease does not cover the exact frozen repair roots"
        );
    }
    Ok(lease)
}

fn load_canonical_intent(path: &Path) -> Result<Option<ProfileRepairIntent>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read repair intent {}", path.display()));
        }
    };
    let intent: ProfileRepairIntent = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode repair intent {}", path.display()))?;
    if canonical_json_bytes(&intent, "profile repair intent")? != bytes {
        anyhow::bail!("profile repair intent is not canonical: {}", path.display());
    }
    Ok(Some(intent))
}

fn load_canonical_receipt(path: &Path) -> Result<Option<(ProfileRepairReceipt, Vec<u8>)>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read repair receipt {}", path.display()));
        }
    };
    let receipt: ProfileRepairReceipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode repair receipt {}", path.display()))?;
    if canonical_json_bytes(&receipt, "profile repair receipt")? != bytes {
        anyhow::bail!(
            "profile repair receipt is not canonical: {}",
            path.display()
        );
    }
    Ok(Some((receipt, bytes)))
}

fn roots_to_pin(roots: &ProfileIndexRoots) -> Result<RootPairPin> {
    let by_pubkey = roots
        .by_pubkey
        .as_ref()
        .context("profile-by-pubkey root is missing")?;
    let search = roots
        .search
        .as_ref()
        .context("profile-search root is missing")?;
    let by_pubkey_file_sha256 = roots
        .by_pubkey_file_sha256
        .clone()
        .context("profile-by-pubkey root-file SHA-256 is missing")?;
    let search_file_sha256 = roots
        .search_file_sha256
        .clone()
        .context("profile-search root-file SHA-256 is missing")?;
    Ok(RootPairPin {
        by_pubkey: cid_to_nhash(by_pubkey)?,
        by_pubkey_file_sha256,
        search: cid_to_nhash(search)?,
        search_file_sha256,
    })
}

fn pin_to_roots(pin: &RootPairPin) -> Result<ProfileIndexRoots> {
    let by_pubkey =
        parse_root_text(&pin.by_pubkey).context("parse pinned by-pubkey repair root")?;
    let search = parse_root_text(&pin.search).context("parse pinned profile-search repair root")?;
    if profile_index_root_file_sha256(&by_pubkey)? != pin.by_pubkey_file_sha256
        || profile_index_root_file_sha256(&search)? != pin.search_file_sha256
    {
        anyhow::bail!("profile repair root-file digest does not match its pinned CID");
    }
    Ok(ProfileIndexRoots {
        by_pubkey: Some(by_pubkey),
        search: Some(search),
        by_pubkey_file_sha256: Some(pin.by_pubkey_file_sha256.clone()),
        search_file_sha256: Some(pin.search_file_sha256.clone()),
    })
}

fn validate_required_pubkeys(mut required: Vec<String>) -> Result<Vec<String>> {
    if required.is_empty() {
        anyhow::bail!("profile repair requires at least one explicit required pubkey");
    }
    for pubkey in &required {
        if pubkey.len() != 64
            || !pubkey
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            anyhow::bail!("required profile pubkey is not canonical lowercase hex: {pubkey}");
        }
    }
    required.sort();
    required.dedup();
    Ok(required)
}

fn retained_profiles(
    spool: &BulkProjectionSpool,
    decisions: &BTreeMap<String, Option<u32>>,
    required_pubkeys: &[String],
) -> Result<(Vec<Event>, String)> {
    let records = spool.retained_profile_records()?;
    if records.is_empty() {
        anyhow::bail!("bulk spool contains no retained kind-0 winner");
    }
    let mut digest = Sha256::new();
    digest.update(b"iris-social/bulk-profile-repair-corpus@1\0");
    let mut events = Vec::with_capacity(records.len());
    for (pubkey, record) in &records {
        if record.event.kind != 0 || record.event.pubkey != *pubkey {
            anyhow::bail!("retained profile spool record is not keyed by its metadata author");
        }
        match decisions.get(pubkey) {
            Some(Some(_)) => {}
            Some(None) => {
                anyhow::bail!("bulk spool retained excluded profile author {pubkey}");
            }
            None => anyhow::bail!("rank decisions omitted retained profile author {pubkey}"),
        }
        let event = record
            .event
            .to_nostr_sdk_event()
            .with_context(|| format!("decode retained profile event {}", record.event.id))?;
        event
            .verify()
            .with_context(|| format!("verify retained signed profile event {}", event.id))?;
        let encoded =
            rmp_serde::to_vec_named(record).context("encode retained profile corpus record")?;
        digest.update((pubkey.len() as u64).to_be_bytes());
        digest.update(pubkey.as_bytes());
        digest.update((encoded.len() as u64).to_be_bytes());
        digest.update(encoded);
        events.push(event);
    }
    for required in required_pubkeys {
        if !records.contains_key(required) {
            anyhow::bail!("required profile pubkey is absent from retained spool: {required}");
        }
    }
    Ok((events, hex::encode(digest.finalize())))
}

fn require_exact_bytes(label: &str, path: &Path, expected: &[u8]) -> Result<()> {
    let actual =
        std::fs::read(path).with_context(|| format!("re-read {label} {}", path.display()))?;
    if actual != expected {
        anyhow::bail!("{label} changed during profile repair");
    }
    Ok(())
}

struct ProfileRepairAuthority<'a> {
    data_dir: &'a Path,
    staging_data_dir: &'a Path,
    state_sha256: &'a str,
    stage_state_sha256: &'a str,
    policy_sha256: &'a str,
    spool_data_sha256: &'a str,
    options: &'a BulkProfileRepairOptions,
    profile_rank_provenance: &'a ProfileDistanceProvenance,
    built_roots: &'a BTreeMap<u8, String>,
    profile_records: usize,
    profile_records_sha256: &'a str,
    required_profile_pubkeys: &'a [String],
    event_index_parity: &'a BulkProjectionExactIndexParityEvidence,
}

fn validate_intent_authority(
    intent: &ProfileRepairIntent,
    authority: ProfileRepairAuthority<'_>,
) -> Result<()> {
    let expected_data_dir = authority
        .data_dir
        .canonicalize()
        .context("canonicalize profile repair data directory")?;
    let expected_staging_dir = authority
        .staging_data_dir
        .canonicalize()
        .context("canonicalize profile repair staging directory")?;
    if intent.format != PROFILE_REPAIR_FORMAT
        || Path::new(&intent.data_dir) != expected_data_dir
        || Path::new(&intent.staging_data_dir) != expected_staging_dir
        || intent.state_sha256 != authority.state_sha256
        || intent.stage_state_sha256 != authority.stage_state_sha256
        || intent.policy_sha256 != authority.policy_sha256
        || intent.spool_data_sha256 != authority.spool_data_sha256
        || intent.profile_rank_decisions_file_sha256
            != authority
                .options
                .expected_profile_rank_decisions_file_sha256
        || intent.profile_rank_decisions_report_sha256
            != authority
                .options
                .expected_profile_rank_decisions_report_sha256
        || &intent.profile_rank_provenance != authority.profile_rank_provenance
        || intent.replayed_author_count != authority.options.expected_replayed_author_count
        || intent.full_author_count != authority.options.expected_full_author_count
        || intent.btree_order != authority.options.btree_order
        || &intent.built_roots != authority.built_roots
        || intent.profile_records != authority.profile_records
        || intent.profile_records_sha256 != authority.profile_records_sha256
        || intent.required_profile_pubkeys != authority.required_profile_pubkeys
        || &intent.event_index_parity != authority.event_index_parity
        || intent.old_roots.by_pubkey_file_sha256
            != authority
                .options
                .expected_profiles_by_pubkey_root_file_sha256
        || intent.old_roots.search_file_sha256
            != authority.options.expected_profile_search_root_file_sha256
    {
        anyhow::bail!("existing profile repair intent differs from exact pinned authority");
    }
    pin_to_roots(&intent.old_roots)?;
    pin_to_roots(&intent.new_roots)?;
    validate_pool_catalog_pin(
        "durable intent Pool snapshot",
        &intent.prepublish_pool_catalog,
    )?;
    validate_exact_event_index_parity_evidence(
        &intent.event_index_parity,
        &intent.built_roots,
        intent.btree_order,
    )?;
    require_sha256(
        "durable intent retention lease SHA-256",
        &intent.retention_lease_sha256,
    )?;
    Ok(())
}

fn validate_completed_receipt_authority(
    intent: &ProfileRepairIntent,
    receipt: &ProfileRepairReceipt,
    data_dir: &Path,
    options: &BulkProfileRepairOptions,
    required_profile_pubkeys: &[String],
) -> Result<()> {
    let data_dir = data_dir
        .canonicalize()
        .context("canonicalize completed profile repair data directory")?;
    let staging_data_dir = options
        .staging_data_dir
        .canonicalize()
        .context("canonicalize completed profile repair staging directory")?;
    let intent_bytes = canonical_json_bytes(intent, "profile repair intent")?;
    if intent.format != PROFILE_REPAIR_FORMAT
        || Path::new(&intent.data_dir) != data_dir
        || Path::new(&intent.staging_data_dir) != staging_data_dir
        || intent.state_sha256 != options.expected_state_sha256
        || intent.stage_state_sha256 != options.expected_stage_state_sha256
        || intent.policy_sha256 != options.expected_policy_sha256
        || intent.spool_data_sha256 != options.expected_spool_data_sha256
        || intent.profile_rank_decisions_file_sha256
            != options.expected_profile_rank_decisions_file_sha256
        || intent.profile_rank_decisions_report_sha256
            != options.expected_profile_rank_decisions_report_sha256
        || intent.replayed_author_count != options.expected_replayed_author_count
        || intent.full_author_count != options.expected_full_author_count
        || intent.btree_order != options.btree_order
        || intent.required_profile_pubkeys != required_profile_pubkeys
        || intent.old_roots.by_pubkey_file_sha256
            != options.expected_profiles_by_pubkey_root_file_sha256
        || intent.old_roots.search_file_sha256 != options.expected_profile_search_root_file_sha256
    {
        anyhow::bail!("completed profile repair differs from exact requested authority");
    }
    if intent.built_roots.len() != 9
        || intent.built_roots.keys().copied().collect::<Vec<_>>() != (0u8..9).collect::<Vec<_>>()
        || intent.built_roots.values().any(String::is_empty)
    {
        anyhow::bail!("completed profile repair intent has invalid event-root authority");
    }
    for encoded in intent.built_roots.values() {
        let root = parse_root_text(encoded).context("parse completed repair event root")?;
        if cid_to_nhash(&root)? != *encoded {
            anyhow::bail!("completed profile repair event root is not canonical nhash text");
        }
    }
    pin_to_roots(&intent.old_roots)?;
    pin_to_roots(&intent.new_roots)?;
    pin_to_roots(&receipt.installed_roots)?;
    validate_pool_catalog_pin(
        "completed intent Pool snapshot",
        &intent.prepublish_pool_catalog,
    )?;
    validate_pool_catalog_pin(
        "completed receipt Pool snapshot",
        &receipt.completion_pool_catalog,
    )?;
    require_sha256(
        "completed repair retention lease SHA-256",
        &intent.retention_lease_sha256,
    )?;
    validate_exact_event_index_parity_evidence(
        &intent.event_index_parity,
        &intent.built_roots,
        intent.btree_order,
    )?;
    validate_exact_event_index_parity_evidence(
        &receipt.event_index_parity,
        &intent.built_roots,
        intent.btree_order,
    )?;
    if receipt.format != PROFILE_REPAIR_RECEIPT_FORMAT
        || receipt.intent_sha256 != stage_bytes_sha256(&intent_bytes)
        || receipt.state_sha256 != intent.state_sha256
        || receipt.stage_state_sha256 != intent.stage_state_sha256
        || receipt.spool_data_sha256 != intent.spool_data_sha256
        || receipt.profile_records != intent.profile_records
        || receipt.profile_records_sha256 != intent.profile_records_sha256
        || receipt.required_profile_pubkeys != intent.required_profile_pubkeys
        || receipt.retention_lease_sha256 != intent.retention_lease_sha256
        || receipt.event_index_parity != intent.event_index_parity
        || receipt.installed_roots != intent.new_roots
        || receipt.postpublish_audit != intent.prepublish_audit
    {
        anyhow::bail!("profile repair receipt does not exactly complete its durable intent");
    }
    Ok(())
}

fn publish_receipt_output(out: Option<&Path>, receipt_bytes: &[u8]) -> Result<()> {
    match out {
        Some(path) if path != Path::new("-") => {
            persist_immutable_bytes(path, receipt_bytes, "profile repair output")?;
        }
        _ => {
            print!("{}", String::from_utf8_lossy(receipt_bytes));
        }
    }
    Ok(())
}

pub(crate) async fn repair_bulk_projection_profiles<F>(
    data_dir: &Path,
    options: BulkProfileRepairOptions,
    mut open_writer: F,
) -> Result<()>
where
    F: FnMut() -> Result<(HashtreeStore, Arc<SocialGraphStore>)>,
{
    for (label, pin) in [
        (
            "bulk projection state SHA-256",
            options.expected_state_sha256.as_str(),
        ),
        (
            "staging state SHA-256",
            options.expected_stage_state_sha256.as_str(),
        ),
        (
            "crawl policy SHA-256",
            options.expected_policy_sha256.as_str(),
        ),
        (
            "bulk spool data SHA-256",
            options.expected_spool_data_sha256.as_str(),
        ),
        (
            "event-blob repair receipt SHA-256",
            options.expected_event_blob_repair_receipt_sha256.as_str(),
        ),
        (
            "profile rank-decisions file SHA-256",
            options.expected_profile_rank_decisions_file_sha256.as_str(),
        ),
        (
            "profile rank-decisions report SHA-256",
            options
                .expected_profile_rank_decisions_report_sha256
                .as_str(),
        ),
        (
            "old profile-by-pubkey root-file SHA-256",
            options
                .expected_profiles_by_pubkey_root_file_sha256
                .as_str(),
        ),
        (
            "old profile-search root-file SHA-256",
            options.expected_profile_search_root_file_sha256.as_str(),
        ),
    ] {
        require_sha256(label, pin)?;
    }
    if options.expected_replayed_author_count == 0
        || options.expected_replayed_author_count > options.expected_full_author_count
        || options.btree_order < 2
    {
        anyhow::bail!(
            "profile repair requires a nonzero replay watermark not above the full author count"
        );
    }
    if options
        .out
        .as_deref()
        .is_some_and(|path| path != Path::new("-") && !path.is_absolute())
    {
        anyhow::bail!("profile repair output path must be absolute or `-`");
    }
    let required_profile_pubkeys =
        validate_required_pubkeys(options.required_profile_pubkeys.clone())?;
    let trusted_event_repair = load_trusted_event_blob_repair_receipt(
        &options.event_blob_repair_receipt,
        &options.expected_event_blob_repair_receipt_sha256,
    )?;
    hashtree_cli::socialgraph::bootstrap_profile_root_pair_transaction_lock(data_dir)
        .context("bootstrap legacy profile root-pair transaction lock")?;
    let (intent_path, receipt_path) = repair_paths(data_dir);
    let completion_path = profile_repair_completion_path(data_dir);
    let existing_intent = load_canonical_intent(&intent_path)?;
    let existing_receipt = load_canonical_receipt(&receipt_path)?;
    if let Some((receipt, receipt_bytes)) = existing_receipt.as_ref() {
        let intent = existing_intent
            .as_ref()
            .context("profile repair receipt exists without its durable intent")?;
        validate_completed_receipt_authority(
            intent,
            receipt,
            data_dir,
            &options,
            &required_profile_pubkeys,
        )?;
        validate_trusted_event_blob_repair_evidence(
            &trusted_event_repair,
            &intent.built_roots,
            intent.btree_order,
        )?;
        if intent.event_index_parity != trusted_event_repair.event_index_parity {
            anyhow::bail!(
                "completed profile repair event evidence differs from the pinned event-blob repair receipt"
            );
        }
        let installed_roots = pin_to_roots(&intent.new_roots)?;
        load_profile_repair_retention_lease(
            data_dir,
            &intent.retention_lease_sha256,
            &intent.built_roots,
            &installed_roots,
        )
        .context("validate completed profile repair retention lease")?;
        let intent_bytes = canonical_json_bytes(intent, "profile repair intent")?;
        let completion_bytes =
            profile_repair_completion_witness_bytes(&intent_bytes, receipt_bytes)?;
        match std::fs::read(&completion_path) {
            Ok(existing) if existing == completion_bytes => {}
            Ok(_) => anyhow::bail!(
                "profile repair completion already exists with different bytes at {}",
                completion_path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (completion_store, completion_graph) = open_writer()
                    .context("open exact writable store for repair completion recovery")?;
                let prepared = PreparedProfileIndexRepair::from_roots(
                    pin_to_roots(&intent.old_roots)?,
                    installed_roots,
                );
                let authority = completion_graph
                    .authorize_completed_profile_index_repair(
                        &prepared,
                        &intent_bytes,
                        receipt_bytes,
                    )
                    .context("authorize exact completed profile repair recovery")?;
                let completion_publication = completion_graph
                    .hold_completed_profile_index_repair(&prepared, authority)
                    .context("hold exact completed profile roots for completion recovery")?;
                persist_immutable_bytes(
                    &completion_path,
                    &completion_bytes,
                    "profile repair completion",
                )?;
                completion_publication.require_unchanged()?;
                drop(completion_publication);
                drop(completion_graph);
                drop(completion_store);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "read profile repair completion {}",
                        completion_path.display()
                    )
                });
            }
        }
        let publication_fence =
            hashtree_cli::socialgraph::acquire_profile_publication_fence_guard(data_dir)
                .await
                .context("drain profile publishers before restoring completed repair fence")?;
        let fence_path = hashtree_cli::socialgraph::profile_publication_fence_path(data_dir);
        persist_immutable_bytes(
            &fence_path,
            PROFILE_PUBLICATION_FENCE_BYTES,
            "profile publication fence",
        )?;
        drop(publication_fence);
        publish_receipt_output(options.out.as_deref(), receipt_bytes)?;
        return Ok(());
    }
    let (state_path, spool_path) = bulk_paths(data_dir);
    let stage_state_path = options
        .staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_STATE_FILE);
    let spool_data_path = spool_path.join("data.mdb");
    let state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("read bulk state {}", state_path.display()))?;
    let state_sha256 = stage_bytes_sha256(&state_bytes);
    if state_sha256 != options.expected_state_sha256 {
        anyhow::bail!(
            "bulk state SHA-256 mismatch: expected {}, found {}",
            options.expected_state_sha256,
            state_sha256
        );
    }
    let state: BulkProjectionState =
        serde_json::from_slice(&state_bytes).context("decode bulk projection state")?;
    if state.version != BULK_PROJECTION_VERSION
        || state.complete_root.is_some()
        || state.segment_event_offset != 0
        || state.next_author != options.expected_replayed_author_count
        || state.policy.author_count != options.expected_full_author_count
        || state.policy.max_authors != options.expected_full_author_count
        || state.built_roots.len() != 9
        || state.built_roots.keys().copied().collect::<Vec<_>>() != (0u8..9).collect::<Vec<_>>()
        || state.built_roots.values().any(String::is_empty)
    {
        anyhow::bail!(
            "profile repair requires terminal v2 replay, exactly nine roots, and no complete root"
        );
    }
    for (stable_id, encoded) in &state.built_roots {
        let root = parse_root_text(encoded)
            .with_context(|| format!("parse pinned event-index root {stable_id}"))?;
        if cid_to_nhash(&root)? != *encoded {
            anyhow::bail!("event-index root {stable_id} is not canonical nhash text");
        }
    }
    validate_trusted_event_blob_repair_evidence(
        &trusted_event_repair,
        &state.built_roots,
        options.btree_order,
    )?;
    let policy_sha256 = stage_bytes_sha256(
        &serde_json::to_vec(&state.policy).context("encode pinned crawl policy")?,
    );
    if policy_sha256 != options.expected_policy_sha256 {
        anyhow::bail!(
            "crawl policy SHA-256 mismatch: expected {}, found {}",
            options.expected_policy_sha256,
            policy_sha256
        );
    }
    let stage_state_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("read staging state {}", stage_state_path.display()))?;
    let stage_state_sha256 = stage_bytes_sha256(&stage_state_bytes);
    if stage_state_sha256 != options.expected_stage_state_sha256 {
        anyhow::bail!(
            "staging state SHA-256 mismatch: expected {}, found {}",
            options.expected_stage_state_sha256,
            stage_state_sha256
        );
    }
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&stage_state_bytes).context("decode staging state")?;
    if stage.version != super::super::STAGE_FORMAT_VERSION {
        anyhow::bail!(
            "staging state version {} differs from required version {}",
            stage.version,
            super::super::STAGE_FORMAT_VERSION
        );
    }
    validate_terminal_stage_state(&state, &stage)?;

    let spool_data_sha256 = hash_file(&spool_data_path)?;
    if spool_data_sha256 != options.expected_spool_data_sha256 {
        anyhow::bail!(
            "bulk spool data SHA-256 mismatch: expected {}, found {}",
            options.expected_spool_data_sha256,
            spool_data_sha256
        );
    }
    let rank_authority = load_pinned_profile_rank_decisions(
        &options.profile_rank_decisions_file,
        &options.expected_profile_rank_decisions_file_sha256,
        &options.profile_rank_decisions_report,
        &options.expected_profile_rank_decisions_report_sha256,
    )?;
    require_profile_rank_policy_binding(
        Some(&rank_authority),
        &state.policy.author_allowlist_sha256,
        state.policy.author_count,
    )?;
    let spool = BulkProjectionSpool::open_read_only(&spool_path)?;
    let (profiles, profile_records_sha256) =
        retained_profiles(&spool, &rank_authority.decisions, &required_profile_pubkeys)?;
    let profile_records = profiles.len();

    let publication_fence =
        hashtree_cli::socialgraph::acquire_profile_publication_fence_guard(data_dir)
            .await
            .context("drain in-flight external profile-root publications")?;
    let fence_path = hashtree_cli::socialgraph::profile_publication_fence_path(data_dir);
    persist_immutable_bytes(
        &fence_path,
        PROFILE_PUBLICATION_FENCE_BYTES,
        "profile publication fence",
    )?;
    drop(publication_fence);
    let intent_was_existing = existing_intent.is_some();
    if !intent_was_existing {
        let event_completion_store = open_exact_durable_pool(data_dir)?;
        let current_catalog = pin_committed_pool_catalog(&event_completion_store)?;
        if current_catalog != trusted_event_repair.completion_pool_catalog {
            anyhow::bail!(
                "PoolStore catalog differs from the exact event-blob repair completion snapshot"
            );
        }
        drop(event_completion_store);
    }
    let (intent, prepared, verified_pool_catalog) = if let Some(intent) = existing_intent {
        let verification_store = open_exact_durable_pool(data_dir)?;
        let pool_catalog = pin_committed_pool_catalog(&verification_store)?;
        let event_index_parity = trusted_event_repair.event_index_parity.clone();
        validate_intent_authority(
            &intent,
            ProfileRepairAuthority {
                data_dir,
                staging_data_dir: &options.staging_data_dir,
                state_sha256: &state_sha256,
                stage_state_sha256: &stage_state_sha256,
                policy_sha256: &policy_sha256,
                spool_data_sha256: &spool_data_sha256,
                options: &options,
                profile_rank_provenance: &rank_authority.evidence,
                built_roots: &state.built_roots,
                profile_records,
                profile_records_sha256: &profile_records_sha256,
                required_profile_pubkeys: &required_profile_pubkeys,
                event_index_parity: &event_index_parity,
            },
        )?;
        let prepared = PreparedProfileIndexRepair::from_roots(
            pin_to_roots(&intent.old_roots)?,
            pin_to_roots(&intent.new_roots)?,
        );
        load_profile_repair_retention_lease(
            data_dir,
            &intent.retention_lease_sha256,
            &state.built_roots,
            prepared.new_roots(),
        )
        .context("validate durable repair retention lease")?;
        let (audit, _) = audit_profile_indexes_at_roots(
            &spool,
            Arc::clone(&verification_store),
            prepared.new_roots(),
            Some(&rank_authority.decisions),
        )
        .await
        .context("re-audit unpublished roots from existing repair intent")?;
        if audit != intent.prepublish_audit {
            anyhow::bail!("unpublished profile repair audit differs from durable intent");
        }
        if pin_committed_pool_catalog(&verification_store)? != pool_catalog {
            anyhow::bail!("PoolStore catalog changed while re-auditing durable repair intent");
        }
        (intent, prepared, pool_catalog)
    } else {
        let old_roots = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
        if old_roots.by_pubkey_file_sha256.as_deref()
            != Some(&options.expected_profiles_by_pubkey_root_file_sha256)
            || old_roots.search_file_sha256.as_deref()
                != Some(&options.expected_profile_search_root_file_sha256)
        {
            anyhow::bail!("published profile roots differ from explicit pre-repair SHA-256 pins");
        }
        let (build_store, build_graph) =
            open_writer().context("open exact writable store for profile repair build")?;
        let retention_publication = build_store
            .acquire_profile_repair_retention_publication_guard()
            .context("drain local retention before building unpublished repair roots")?;
        let prepared = build_graph
            .build_unpublished_profile_index_repair_with_frozen_distances(
                &profiles,
                &rank_authority.decisions,
            )
            .context("build and force-sync complete unpublished profile repair roots")?;
        run_profile_repair_boundary_probe("after-unpublished-profile-roots-built")?;
        let retention_lease = build_profile_repair_retention_lease(
            data_dir,
            &options.staging_data_dir,
            &state_sha256,
            &stage_state_sha256,
            &policy_sha256,
            &spool_data_sha256,
            &options,
            profile_records,
            &profile_records_sha256,
            &required_profile_pubkeys,
            &state.built_roots,
            prepared.new_roots(),
        )?;
        let retention_lease_bytes = retention_lease.canonical_bytes()?;
        let retention_lease_sha256 = stage_bytes_sha256(&retention_lease_bytes);
        persist_immutable_bytes(
            &build_store.profile_repair_retention_lease_path(),
            &retention_lease_bytes,
            "profile repair retention lease",
        )?;
        build_store
            .validate_profile_repair_retention_lease(&retention_lease_sha256)
            .context("validate newly published profile repair retention lease")?;
        run_profile_repair_boundary_probe("after-retention-lease-validated")?;
        drop(retention_publication);
        drop(build_graph);
        drop(build_store);
        run_profile_repair_boundary_probe("after-retention-publication-released")?;
        if prepared.old_roots() != &old_roots {
            anyhow::bail!("published profile roots changed while preparing repair roots");
        }
        let verification_store = open_exact_durable_pool(data_dir)?;
        let pool_catalog = pin_committed_pool_catalog(&verification_store)?;
        let event_index_parity = trusted_event_repair.event_index_parity.clone();
        let (prepublish_audit, _) = audit_profile_indexes_at_roots(
            &spool,
            Arc::clone(&verification_store),
            prepared.new_roots(),
            Some(&rank_authority.decisions),
        )
        .await
        .context("exhaustively audit unpublished profile repair roots")?;
        require_exact_bytes("bulk projection state", &state_path, &state_bytes)?;
        require_exact_bytes("staging state", &stage_state_path, &stage_state_bytes)?;
        recheck_trusted_profile_rank_decisions(Some(&rank_authority))?;
        if pin_committed_pool_catalog(&verification_store)? != pool_catalog {
            anyhow::bail!("PoolStore catalog changed while auditing unpublished repair roots");
        }
        if hashtree_cli::socialgraph::read_profile_index_roots(data_dir)? != old_roots {
            anyhow::bail!("published profile roots changed before durable repair intent");
        }
        let intent = ProfileRepairIntent {
            format: PROFILE_REPAIR_FORMAT.to_string(),
            data_dir: data_dir
                .canonicalize()
                .context("canonicalize repair data directory")?
                .to_string_lossy()
                .into_owned(),
            staging_data_dir: options
                .staging_data_dir
                .canonicalize()
                .context("canonicalize repair staging directory")?
                .to_string_lossy()
                .into_owned(),
            state_sha256: state_sha256.clone(),
            stage_state_sha256: stage_state_sha256.clone(),
            policy_sha256: policy_sha256.clone(),
            spool_data_sha256: spool_data_sha256.clone(),
            profile_rank_decisions_file_sha256: options
                .expected_profile_rank_decisions_file_sha256
                .clone(),
            profile_rank_decisions_report_sha256: options
                .expected_profile_rank_decisions_report_sha256
                .clone(),
            profile_rank_provenance: rank_authority.evidence.clone(),
            replayed_author_count: options.expected_replayed_author_count,
            full_author_count: options.expected_full_author_count,
            btree_order: options.btree_order,
            built_roots: state.built_roots.clone(),
            profile_records,
            profile_records_sha256: profile_records_sha256.clone(),
            required_profile_pubkeys: required_profile_pubkeys.clone(),
            retention_lease_sha256,
            prepublish_pool_catalog: pool_catalog.clone(),
            event_index_parity,
            old_roots: roots_to_pin(prepared.old_roots())?,
            new_roots: roots_to_pin(prepared.new_roots())?,
            prepublish_audit,
        };
        (intent, prepared, pool_catalog)
    };

    let intent_bytes = canonical_json_bytes(&intent, "profile repair intent")?;
    let (commit_store, commit_graph) =
        open_writer().context("reopen exact writable store for profile repair publication")?;
    let publication = commit_graph
        .commit_prepared_profile_index_repair_held_with(&prepared, || {
            require_exact_bytes("bulk projection state", &state_path, &state_bytes)?;
            require_exact_bytes("staging state", &stage_state_path, &stage_state_bytes)?;
            recheck_trusted_profile_rank_decisions(Some(&rank_authority))?;
            if hash_file(&spool_data_path)? != spool_data_sha256 {
                anyhow::bail!("bulk spool data changed before durable profile repair intent");
            }
            persist_immutable_bytes(&intent_path, &intent_bytes, "profile repair intent")?;
            if !intent_was_existing {
                run_profile_repair_boundary_probe("after-durable-intent")?;
            }
            commit_graph
                .authorize_prepared_profile_index_repair(&prepared, &intent_bytes)
                .context("authorize exact durable profile repair intent")
        })
        .context("CAS-commit exact profile repair root pair")?;
    run_profile_repair_boundary_probe("after-root-pair-commit")?;
    let installed = publication.installed_roots().clone();
    if installed != *prepared.new_roots() {
        anyhow::bail!("published profile roots differ from the exact prepared repair pair");
    }
    drop(commit_graph);
    drop(commit_store);
    let postpublish_store = open_exact_durable_pool(data_dir)?;
    let postpublish_pool_catalog = pin_committed_pool_catalog(&postpublish_store)?;
    if postpublish_pool_catalog != verified_pool_catalog {
        anyhow::bail!(
            "PoolStore catalog changed between exact pre-publication audit and root commit"
        );
    }
    let (postpublish_audit, _) = audit_profile_indexes_at_roots(
        &spool,
        Arc::clone(&postpublish_store),
        &installed,
        Some(&rank_authority.decisions),
    )
    .await
    .context("exhaustively audit installed profile repair roots")?;
    if postpublish_audit != intent.prepublish_audit {
        anyhow::bail!("post-publication profile audit differs from pre-publication evidence");
    }
    for required in &required_profile_pubkeys {
        let event = profiles
            .iter()
            .find(|event| event.pubkey.to_hex() == *required)
            .with_context(|| format!("required retained profile disappeared: {required}"))?;
        let entry = hashtree_cli::socialgraph::validate_profile_indexes_at_roots(
            Arc::clone(&postpublish_store),
            &installed,
            event,
        )
        .await
        .with_context(|| format!("explicitly validate required repaired profile {required}"))?;
        let expected_distance = rank_authority
            .decisions
            .get(required)
            .copied()
            .flatten()
            .context("required repaired profile lost its eligible rank")?;
        if entry.follow_distance != Some(expected_distance) {
            anyhow::bail!("required repaired profile has the wrong frozen rank: {required}");
        }
    }

    require_exact_bytes("bulk projection state", &state_path, &state_bytes)?;
    require_exact_bytes("staging state", &stage_state_path, &stage_state_bytes)?;
    recheck_trusted_profile_rank_decisions(Some(&rank_authority))?;
    if hash_file(&spool_data_path)? != spool_data_sha256 {
        anyhow::bail!("bulk spool data changed during profile repair");
    }
    if pin_committed_pool_catalog(&postpublish_store)? != postpublish_pool_catalog {
        anyhow::bail!("PoolStore catalog changed during post-publication repair audit");
    }
    let receipt = ProfileRepairReceipt {
        format: PROFILE_REPAIR_RECEIPT_FORMAT.to_string(),
        intent_sha256: stage_bytes_sha256(&intent_bytes),
        state_sha256,
        stage_state_sha256,
        spool_data_sha256,
        profile_records,
        profile_records_sha256,
        required_profile_pubkeys,
        retention_lease_sha256: intent.retention_lease_sha256.clone(),
        completion_pool_catalog: postpublish_pool_catalog,
        event_index_parity: intent.event_index_parity.clone(),
        installed_roots: roots_to_pin(&installed)?,
        postpublish_audit,
    };
    let receipt_bytes = canonical_json_bytes(&receipt, "profile repair receipt")?;
    publication.require_unchanged()?;
    persist_immutable_bytes(&receipt_path, &receipt_bytes, "profile repair receipt")?;
    run_profile_repair_boundary_probe("after-durable-receipt")?;
    let completion_bytes = profile_repair_completion_witness_bytes(&intent_bytes, &receipt_bytes)?;
    persist_immutable_bytes(
        &completion_path,
        &completion_bytes,
        "profile repair completion",
    )?;
    publication.require_unchanged()?;
    publish_receipt_output(options.out.as_deref(), &receipt_bytes)?;
    publication.require_unchanged()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    use hashtree_config::StorageBackend;
    use hashtree_core::{collect_hashes, Cid, HashTree, HashTreeConfig};
    use hashtree_index::{BTree, BTreeOptions};
    use hashtree_lmdb::{ReadOnlyPoolStore, SHARED_BLOB_POOL_DIR_NAME};
    use hashtree_nostr::{
        stored_event_from_nostr_sdk_event, NostrEventIndex, NostrEventStore, NostrEventStoreOptions,
    };
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    use super::super::super::{
        persist_stage_state, IndexedNostrCrawlPolicy, StagedNostrCrawlState, STAGE_FORMAT_VERSION,
    };

    struct GeneratedRankAuthority {
        decisions_path: PathBuf,
        decisions_sha256: String,
        report_path: PathBuf,
        report_sha256: String,
    }

    fn read_optional_test_file(path: &Path) -> Option<Vec<u8>> {
        match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read optional test file {}: {error}", path.display()),
        }
    }

    fn generated_rank_authority(
        directory: &Path,
        ranks: &BTreeMap<String, u32>,
        eligible_authors_sha256: &str,
    ) -> GeneratedRankAuthority {
        const FORMAT: &str = "iris-social/profile-search-v3-rank-decisions@1";
        let mut semantic = Sha256::new();
        semantic.update(FORMAT.as_bytes());
        semantic.update(b"\n");
        for (pubkey, rank) in ranks {
            let semantic_row =
                serde_json::to_string(&serde_json::json!([pubkey, "eligible", rank])).unwrap();
            semantic.update(semantic_row.as_bytes());
            semantic.update(b"\n");
        }
        let semantic_sha256 = hex::encode(semantic.finalize());
        let mut decisions_bytes = format!(
            "{{\"format\":\"{FORMAT}\",\"eligibleRanksSha256\":\"{semantic_sha256}\",\"recordCount\":{}}}\n",
            ranks.len()
        )
        .into_bytes();
        for (pubkey, rank) in ranks {
            decisions_bytes.extend_from_slice(
                format!(
                    "{{\"pubkey\":\"{pubkey}\",\"decision\":\"eligible\",\"rankHint\":{rank}}}\n"
                )
                .as_bytes(),
            );
        }
        let decisions_sha256 = stage_bytes_sha256(&decisions_bytes);
        let max_rank = ranks.values().copied().max().unwrap();
        let report = serde_json::json!({
            "format": "iris-social/profile-search-v3-rank-decision-artifacts@1",
            "censusFormat": "iris-social/social-graph-crawl-census@2",
            "socialGraphRoot": "a".repeat(64),
            "socialGraphSha256": "b".repeat(64),
            "eligibleAuthorsSha256": eligible_authors_sha256,
            "overmuteThreshold": 1,
            "maxDistance": max_rank,
            "rankPolicy": "follow-distance@1",
            "exclusionPolicy": "all-nonselected-graph-identities@1",
            "recordCount": ranks.len(),
            "eligibleCount": ranks.len(),
            "excludedCount": 0,
            "reachableCount": ranks.len(),
            "reachableOvermutedCount": 0,
            "distanceExcludedCount": 0,
            "unreachableCount": 0,
            "allGraphOvermutedCount": 0,
            "rankDecisionsSha256": semantic_sha256,
            "rankDecisionsFileSha256": decisions_sha256,
        });
        let report_bytes =
            format!("{}\n", serde_json::to_string_pretty(&report).unwrap()).into_bytes();
        let report_sha256 = stage_bytes_sha256(&report_bytes);
        let decisions_path = directory.join("rank-decisions.jsonl");
        let report_path = directory.join("rank-report.json");
        std::fs::write(&decisions_path, decisions_bytes).unwrap();
        std::fs::write(&report_path, report_bytes).unwrap();
        GeneratedRankAuthority {
            decisions_path,
            decisions_sha256,
            report_path,
            report_sha256,
        }
    }

    fn policy(pubkeys: &[String]) -> IndexedNostrCrawlPolicy {
        let author_lines = pubkeys
            .iter()
            .map(|pubkey| format!("{pubkey}\n"))
            .collect::<String>();
        IndexedNostrCrawlPolicy {
            base_root: None,
            author_allowlist_sha256: stage_bytes_sha256(author_lines.as_bytes()),
            author_count: pubkeys.len(),
            relays: vec!["wss://relay.example".to_string()],
            require_all_relays: false,
            max_events_seen: None,
            max_authors: pubkeys.len(),
            max_follow_distance: Some(4),
            max_live_bytes: 1_000_000,
            author_batch_size: 1,
            checkpoint_authors: 1,
            per_author_event_limit: 256,
            per_author_kind_event_limit: None,
            per_author_live_bytes: Some(64 * 1024 * 1024),
            fetch_timeout_millis: 30_000,
            relay_event_max_bytes: Some(1024 * 1024),
            global_relay_scan: false,
            full_author_history: true,
            negentropy_only: false,
            relay_page_size: 1_000,
            max_relay_pages: 67,
            kinds: Some(vec![0, 1, 30_000]),
        }
    }

    const REPAIR_CHILD_CONTEXT_ENV: &str = "HTREE_TEST_PROFILE_REPAIR_CHILD_CONTEXT";
    const REPAIR_CHILD_BOUNDARY_ENV: &str = "HTREE_TEST_PROFILE_REPAIR_CHILD_BOUNDARY";
    const REPAIR_CHILD_PAUSE_DIR_ENV: &str = "HTREE_TEST_PROFILE_REPAIR_CHILD_PAUSE_DIR";
    const REPAIR_TEST_NAME: &str =
        "app::nostr_index::bulk_projection::repair::tests::repairs_real_pool_from_signed_spool_and_reopens_read_only";
    const REPAIR_CHILD_TIMEOUT: Duration = Duration::from_secs(60);
    const REPAIR_CHILD_PAUSE_BOUNDARIES: [&str; 3] = [
        "after-unpublished-profile-roots-built",
        "after-retention-lease-validated",
        "after-retention-publication-released",
    ];

    #[derive(Serialize, Deserialize)]
    struct GeneratedRepairChildContext {
        data_dir: PathBuf,
        options: BulkProfileRepairOptions,
    }

    fn repair_child_marker_path(pause_dir: &Path, boundary: &str, marker: &str) -> PathBuf {
        pause_dir.join(format!("{boundary}.{marker}"))
    }

    fn pause_generated_repair_child(pause_dir: &Path, boundary: &str) -> Result<()> {
        let ready = repair_child_marker_path(pause_dir, boundary, "ready");
        let release = repair_child_marker_path(pause_dir, boundary, "release");
        std::fs::write(&ready, b"ready\n")
            .with_context(|| format!("write generated repair marker {}", ready.display()))?;
        let deadline = Instant::now() + REPAIR_CHILD_TIMEOUT;
        while !release.exists() {
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting to release generated repair boundary {boundary}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    async fn run_generated_repair_child_if_requested() {
        let Some(context_path) = std::env::var_os(REPAIR_CHILD_CONTEXT_ENV) else {
            return;
        };
        let context: GeneratedRepairChildContext =
            serde_json::from_slice(&std::fs::read(context_path).unwrap()).unwrap();
        let boundary = std::env::var(REPAIR_CHILD_BOUNDARY_ENV).unwrap();
        assert!(matches!(
            boundary.as_str(),
            "after-durable-intent" | "after-root-pair-commit" | "after-durable-receipt"
        ));
        let killed_boundary = boundary.clone();
        let pause_dir = std::env::var_os(REPAIR_CHILD_PAUSE_DIR_ENV).map(PathBuf::from);
        let _probe = install_profile_repair_boundary_probe(Arc::new(move |observed| {
            if let Some(pause_dir) = pause_dir.as_deref() {
                if REPAIR_CHILD_PAUSE_BOUNDARIES.contains(&observed) {
                    pause_generated_repair_child(pause_dir, observed)?;
                }
            }
            if observed == killed_boundary {
                #[cfg(unix)]
                {
                    let result = unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                    if result != 0 {
                        anyhow::bail!(
                            "failed to SIGKILL generated repair child: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                    anyhow::bail!("SIGKILL unexpectedly returned to generated repair child");
                }
                #[cfg(not(unix))]
                anyhow::bail!("generated repair SIGKILL test requires Unix");
            }
            Ok(())
        }));
        let data_dir = context.data_dir;
        let mut open_writer = || {
            let store = HashtreeStore::with_options_and_backend(
                &data_dir,
                None,
                0,
                false,
                &StorageBackend::Lmdb,
            )?;
            let local = store.router().local_store();
            let hashtree_cli::storage::LocalStore::Pool(pool) = local.as_ref() else {
                anyhow::bail!("generated repair child did not reopen as PoolStore");
            };
            pool.stop_temperature_worker()?;
            drop(local);
            let graph = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
                &data_dir,
                store.store_arc(),
                Some(128 * 1024 * 1024),
            )?;
            Ok((store, graph))
        };
        repair_bulk_projection_profiles(&data_dir, context.options, &mut open_writer)
            .await
            .expect("generated repair child failed before its SIGKILL boundary");
        panic!("generated repair child completed without reaching {boundary}");
    }

    #[cfg(unix)]
    struct GeneratedRepairChild {
        child: Option<std::process::Child>,
    }

    #[cfg(unix)]
    impl GeneratedRepairChild {
        fn spawn(context_path: &Path, boundary: &str, pause_dir: Option<&Path>) -> Self {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", REPAIR_TEST_NAME, "--nocapture"])
                .env(REPAIR_CHILD_CONTEXT_ENV, context_path)
                .env(REPAIR_CHILD_BOUNDARY_ENV, boundary);
            if let Some(pause_dir) = pause_dir {
                command.env(REPAIR_CHILD_PAUSE_DIR_ENV, pause_dir);
            }
            Self {
                child: Some(command.spawn().unwrap()),
            }
        }

        fn wait_for_boundary(&mut self, pause_dir: &Path, boundary: &str) {
            let ready = repair_child_marker_path(pause_dir, boundary, "ready");
            let deadline = Instant::now() + REPAIR_CHILD_TIMEOUT;
            loop {
                if ready.exists() {
                    return;
                }
                if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                    panic!("generated repair child exited before boundary {boundary}: {status}");
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for generated repair boundary {boundary}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn release_boundary(&self, pause_dir: &Path, boundary: &str) {
            let release = repair_child_marker_path(pause_dir, boundary, "release");
            std::fs::write(&release, b"release\n").unwrap();
        }

        fn wait_for_sigkill(mut self, boundary: &str) {
            use std::os::unix::process::ExitStatusExt;

            let status = self.child.take().unwrap().wait().unwrap();
            assert_eq!(
                status.signal(),
                Some(libc::SIGKILL),
                "generated repair child did not die by SIGKILL at {boundary}: {status}"
            );
        }
    }

    #[cfg(unix)]
    impl Drop for GeneratedRepairChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }

    #[cfg(unix)]
    fn run_generated_repair_until_sigkill(context_path: &Path, boundary: &str) {
        GeneratedRepairChild::spawn(context_path, boundary, None).wait_for_sigkill(boundary);
    }

    #[cfg(unix)]
    fn assert_retention_shared_lock_is_blocked(data_dir: &Path) {
        use std::os::fd::AsRawFd;

        let lock_path = data_dir.join(".retention-roots.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        if result == 0 {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
            panic!(
                "shared retention lock unexpectedly crossed active publication at {}",
                lock_path.display()
            );
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::WouldBlock,
            "shared retention lock failed for an unexpected reason at {}: {error}",
            lock_path.display()
        );
    }

    #[cfg(unix)]
    fn assert_gc_still_blocked(
        data_dir: &Path,
        store: &HashtreeStore,
        orphan_hash: &[u8; 32],
        gc_done: &mpsc::Receiver<Result<hashtree_cli::storage::GcStats>>,
    ) {
        assert_retention_shared_lock_is_blocked(data_dir);
        assert!(
            matches!(gc_done.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "garbage collection crossed the retention-publication lock"
        );
        assert!(
            store.blob_exists(orphan_hash).unwrap(),
            "garbage collection deleted the orphan while publication was locked"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repairs_real_pool_from_signed_spool_and_reopens_read_only() {
        run_generated_repair_child_if_requested().await;
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("projection");
        let staging_data_dir = temp.path().join("staging");
        let evidence_dir = temp.path().join("evidence");
        std::fs::create_dir_all(&evidence_dir).unwrap();
        let store = HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            0,
            false,
            &StorageBackend::Lmdb,
        )
        .unwrap();
        let graph = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
            &data_dir,
            store.store_arc(),
            Some(128 * 1024 * 1024),
        )
        .unwrap();

        let stale = EventBuilder::new(Kind::Metadata, r#"{"name":"stale profile"}"#)
            .custom_created_at(Timestamp::from_secs(1))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        graph
            .sync_profile_index_for_events(std::slice::from_ref(&stale))
            .unwrap();
        let old_roots = hashtree_cli::socialgraph::read_profile_index_roots(&data_dir).unwrap();

        let keys = Keys::generate();
        let profile = EventBuilder::new(
            Kind::Metadata,
            r#"{"display_name":"Pow Respecter","name":"powrespecter"}"#,
        )
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
        let note = EventBuilder::new(Kind::TextNote, "real profile repair note")
            .tags([Tag::parse(["t", "repair"]).unwrap()])
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap();
        let parameterized =
            EventBuilder::new(Kind::Custom(30_000), "real profile repair parameterized")
                .tags([Tag::identifier("repair-article")])
                .custom_created_at(Timestamp::from_secs(30))
                .sign_with_keys(&keys)
                .unwrap();
        let stored = [&profile, &note, &parameterized]
            .into_iter()
            .map(stored_event_from_nostr_sdk_event)
            .collect::<Vec<_>>();
        let event_store = NostrEventStore::with_options(
            store.store_arc(),
            NostrEventStoreOptions {
                btree_order: Some(8),
                ..NostrEventStoreOptions::default()
            },
        );
        let cids = event_store.store_event_blobs(stored.clone()).await.unwrap();
        store.force_sync().unwrap();
        let (state_path, spool_path) = bulk_paths(&data_dir);
        let spool = BulkProjectionSpool::open(&spool_path).unwrap();
        spool.apply(stored.into_iter().zip(cids).collect()).unwrap();

        let mut built_roots = BTreeMap::new();
        for index in NostrEventIndex::ALL {
            let root = spool
                .build_index_root(index, store.store_arc(), 8)
                .await
                .unwrap()
                .with_context(|| format!("generated {} root", index.name()))
                .unwrap();
            built_roots.insert(index.stable_id(), cid_to_nhash(&root).unwrap());
        }
        store.force_sync().unwrap();
        let unreplayed_pubkey = Keys::generate().public_key().to_hex();
        let mut full_author_pubkeys = vec![profile.pubkey.to_hex(), unreplayed_pubkey.clone()];
        full_author_pubkeys.sort();
        let policy = policy(&full_author_pubkeys);
        let state = BulkProjectionState {
            version: BULK_PROJECTION_VERSION,
            author_allowlist_source: Some("http://127.0.0.1/repair-authors".to_string()),
            policy: policy.clone(),
            next_author: 1,
            segment_event_offset: 0,
            events_seen: 3,
            events_selected: 3,
            live_bytes_selected: 123,
            built_roots,
            complete_root: None,
        };
        super::super::persist_bulk_state(&state_path, &state).unwrap();
        let stage = StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: state.author_allowlist_source.clone(),
            policy: policy.clone(),
            next_author: 1,
            events_seen: 3,
            events_selected: 3,
            live_bytes_selected: 123,
        };
        persist_stage_state(&staging_data_dir, &stage).unwrap();
        let ranks = full_author_pubkeys
            .iter()
            .enumerate()
            .map(|(position, pubkey)| (pubkey.clone(), position as u32 + 2))
            .collect::<BTreeMap<_, _>>();
        let rank = generated_rank_authority(&evidence_dir, &ranks, &policy.author_allowlist_sha256);
        let spool_closing = spool.env.clone().prepare_for_closing();
        drop(spool);
        spool_closing.wait();

        let state_sha256 = stage_bytes_sha256(&std::fs::read(&state_path).unwrap());
        let stage_state_path = staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
        let stage_state_sha256 = stage_bytes_sha256(&std::fs::read(&stage_state_path).unwrap());
        let policy_sha256 = stage_bytes_sha256(&serde_json::to_vec(&policy).unwrap());
        let spool_data_sha256 = hash_file(&spool_path.join("data.mdb")).unwrap();
        let orphan_bytes = b"generated repair-vs-gc unleased orphan";
        let orphan_hash = hashtree_core::sha256(orphan_bytes);
        store.put_blob(orphan_bytes).unwrap();
        store.force_sync().unwrap();
        drop(event_store);
        drop(graph);
        drop(store);

        let event_receipt_path = evidence_dir.join("event-repair-receipt.json");
        let event_spool = BulkProjectionSpool::open_read_only(&spool_path).unwrap();
        let event_pool = open_exact_durable_pool(&data_dir).unwrap();
        let event_index_parity = super::super::audit::audit_exact_event_index_parity(
            &event_spool,
            Arc::clone(&event_pool),
            &state.built_roots,
            8,
            64,
        )
        .await
        .unwrap();
        let event_receipt = TrustedEventBlobRepairReceipt {
            format: EVENT_BLOB_REPAIR_RECEIPT_FORMAT.to_string(),
            intent_sha256: "5".repeat(64),
            recovered_records: 0,
            missing_set_sha256: "6".repeat(64),
            completion_pool_catalog: pin_committed_pool_catalog(&event_pool).unwrap(),
            event_index_parity,
        };
        let event_receipt_bytes =
            canonical_json_bytes(&event_receipt, "test event repair receipt").unwrap();
        std::fs::write(&event_receipt_path, &event_receipt_bytes).unwrap();
        let event_receipt_sha256 = stage_bytes_sha256(&event_receipt_bytes);
        drop(event_pool);
        let event_spool_closing = event_spool.env.clone().prepare_for_closing();
        drop(event_spool);
        event_spool_closing.wait();

        let receipt_path = evidence_dir.join("repair-receipt.json");
        let options = BulkProfileRepairOptions {
            staging_data_dir: staging_data_dir.clone(),
            expected_state_sha256: state_sha256,
            expected_stage_state_sha256: stage_state_sha256,
            expected_policy_sha256: policy_sha256,
            expected_spool_data_sha256: spool_data_sha256,
            event_blob_repair_receipt: event_receipt_path,
            expected_event_blob_repair_receipt_sha256: event_receipt_sha256,
            profile_rank_decisions_file: rank.decisions_path,
            expected_profile_rank_decisions_file_sha256: rank.decisions_sha256,
            profile_rank_decisions_report: rank.report_path,
            expected_profile_rank_decisions_report_sha256: rank.report_sha256,
            expected_replayed_author_count: 1,
            expected_full_author_count: 2,
            expected_profiles_by_pubkey_root_file_sha256: old_roots
                .by_pubkey_file_sha256
                .clone()
                .unwrap(),
            expected_profile_search_root_file_sha256: old_roots.search_file_sha256.clone().unwrap(),
            required_profile_pubkeys: vec![profile.pubkey.to_hex()],
            btree_order: 8,
            out: Some(receipt_path.clone()),
        };
        let writer_opens = Cell::new(0usize);
        let mut open_writer = || {
            writer_opens.set(writer_opens.get() + 1);
            let store = HashtreeStore::with_options_and_backend(
                &data_dir,
                None,
                0,
                false,
                &StorageBackend::Lmdb,
            )?;
            let local = store.router().local_store();
            let hashtree_cli::storage::LocalStore::Pool(pool) = local.as_ref() else {
                anyhow::bail!("generated repair store did not reopen as PoolStore");
            };
            pool.stop_temperature_worker()?;
            drop(local);
            let graph = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
                &data_dir,
                store.store_arc(),
                Some(128 * 1024 * 1024),
            )?;
            Ok((store, graph))
        };
        let mut capped_state = state.clone();
        capped_state.policy.max_authors = 1;
        let mut capped_stage = stage.clone();
        capped_stage.policy.max_authors = 1;
        super::super::persist_bulk_state(&state_path, &capped_state).unwrap();
        persist_stage_state(&staging_data_dir, &capped_stage).unwrap();
        let mut capped_options = options.clone();
        capped_options.expected_state_sha256 =
            stage_bytes_sha256(&std::fs::read(&state_path).unwrap());
        capped_options.expected_stage_state_sha256 =
            stage_bytes_sha256(&std::fs::read(&stage_state_path).unwrap());
        capped_options.expected_policy_sha256 =
            stage_bytes_sha256(&serde_json::to_vec(&capped_state.policy).unwrap());
        let error = repair_bulk_projection_profiles(&data_dir, capped_options, &mut open_writer)
            .await
            .expect_err("a crawl capped below the full policy universe must be rejected");
        assert!(error.to_string().contains("requires terminal v2 replay"));
        super::super::persist_bulk_state(&state_path, &state).unwrap();
        persist_stage_state(&staging_data_dir, &stage).unwrap();

        assert_eq!(
            hashtree_cli::socialgraph::read_profile_index_roots(&data_dir).unwrap(),
            old_roots
        );
        let root_pair_lock_path = data_dir.join("socialgraph/profile-root-pair.lock");
        std::fs::remove_file(&root_pair_lock_path).unwrap();
        assert!(
            !root_pair_lock_path.exists(),
            "generated legacy store must begin without the transaction lock"
        );
        let mut wrong_event_receipt_options = options.clone();
        wrong_event_receipt_options.expected_event_blob_repair_receipt_sha256 = "7".repeat(64);
        let error = repair_bulk_projection_profiles(
            &data_dir,
            wrong_event_receipt_options,
            &mut open_writer,
        )
        .await
        .expect_err("wrong event-repair receipt pin must fail before profile mutation");
        assert!(error
            .to_string()
            .contains("event-blob repair receipt SHA-256 differs"));
        assert!(
            !root_pair_lock_path.exists(),
            "wrong event receipt pin must fail before bootstrapping the transaction lock"
        );
        let mut invalid_output_options = options.clone();
        invalid_output_options.out = Some(PathBuf::from("relative-receipt.json"));
        let error =
            repair_bulk_projection_profiles(&data_dir, invalid_output_options, &mut open_writer)
                .await
                .expect_err("relative output path must fail before profile repair mutation");
        assert!(error
            .to_string()
            .contains("output path must be absolute or `-`"));
        assert!(
            !root_pair_lock_path.exists(),
            "invalid output must fail before bootstrapping the transaction lock"
        );
        assert_eq!(
            hash_file(&data_dir.join("socialgraph/profiles-by-pubkey-root.msgpack")).unwrap(),
            old_roots.by_pubkey_file_sha256.clone().unwrap()
        );
        assert_eq!(
            hash_file(&data_dir.join("socialgraph/profile-search-root.msgpack")).unwrap(),
            old_roots.search_file_sha256.clone().unwrap()
        );
        let (intent_path, internal_receipt_path) = repair_paths(&data_dir);
        let internal_completion_path =
            hashtree_cli::socialgraph::profile_repair_completion_path(&data_dir);
        assert!(!intent_path.exists());
        assert!(!internal_receipt_path.exists());
        assert!(!internal_completion_path.exists());
        assert!(!hashtree_cli::socialgraph::profile_publication_fence_path(&data_dir).exists());

        let child_context_path = evidence_dir.join("generated-repair-child-context.json");
        std::fs::write(
            &child_context_path,
            serde_json::to_vec(&GeneratedRepairChildContext {
                data_dir: data_dir.clone(),
                options: options.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let pause_dir = evidence_dir.join("repair-retention-race");
        std::fs::create_dir(&pause_dir).unwrap();
        let gc_store = Arc::new(
            HashtreeStore::with_options_and_backend(
                &data_dir,
                None,
                0,
                false,
                &StorageBackend::Lmdb,
            )
            .unwrap(),
        );
        let gc_local = gc_store.router().local_store();
        let hashtree_cli::storage::LocalStore::Pool(gc_pool) = gc_local.as_ref() else {
            panic!("generated repair GC store did not reopen as PoolStore");
        };
        gc_pool.stop_temperature_worker().unwrap();
        drop(gc_local);
        assert!(gc_store.blob_exists(&orphan_hash).unwrap());

        let mut repair_child = GeneratedRepairChild::spawn(
            &child_context_path,
            "after-durable-intent",
            Some(&pause_dir),
        );
        repair_child.wait_for_boundary(&pause_dir, "after-unpublished-profile-roots-built");
        assert!(
            root_pair_lock_path.is_file(),
            "valid repair must bootstrap the legacy root-pair transaction lock"
        );
        assert!(
            !gc_store.profile_repair_retention_lease_path().exists(),
            "repair published its retention lease before the post-build race boundary"
        );

        let (gc_started_tx, gc_started_rx) = mpsc::sync_channel(0);
        let (gc_done_tx, gc_done_rx) = mpsc::sync_channel(1);
        let gc_thread_store = Arc::clone(&gc_store);
        let gc_thread = std::thread::Builder::new()
            .name("generated-repair-retention-gc".to_string())
            .spawn(move || {
                gc_started_tx.send(()).unwrap();
                gc_done_tx.send(gc_thread_store.gc()).unwrap();
            })
            .unwrap();
        gc_started_rx
            .recv_timeout(REPAIR_CHILD_TIMEOUT)
            .expect("generated GC thread did not start");
        assert_gc_still_blocked(&data_dir, &gc_store, &orphan_hash, &gc_done_rx);

        repair_child.release_boundary(&pause_dir, "after-unpublished-profile-roots-built");
        repair_child.wait_for_boundary(&pause_dir, "after-retention-lease-validated");
        let lease_path = gc_store.profile_repair_retention_lease_path();
        let lease_bytes = std::fs::read(&lease_path).unwrap();
        let lease_sha256 = stage_bytes_sha256(&lease_bytes);
        let lease = gc_store
            .validate_profile_repair_retention_lease(&lease_sha256)
            .expect("repair must fsync and validate its canonical lease before releasing GC");
        assert_eq!(
            lease.roots.len(),
            NostrEventIndex::ALL.len() + 2,
            "repair retention lease must cover all event and profile roots"
        );
        assert_gc_still_blocked(&data_dir, &gc_store, &orphan_hash, &gc_done_rx);

        let protected_tree = HashTree::new(HashTreeConfig::new(gc_store.store_arc()));
        let mut leased_hashes = HashSet::new();
        for (label, encoded) in &lease.roots {
            let root = Cid::parse(encoded)
                .unwrap_or_else(|error| panic!("parse retained root {label}: {error}"));
            if matches!(label.as_str(), "profiles-by-pubkey" | "profile-search") {
                assert!(
                    root.key.is_some(),
                    "profile repair retention must preserve the full encrypted {label} CID"
                );
            }
            let hashes = collect_hashes(&protected_tree, &root, 4)
                .await
                .unwrap_or_else(|error| panic!("collect retained root {label}: {error}"));
            assert!(
                !hashes.is_empty(),
                "retained root {label} produced no generated DAG hashes"
            );
            for hash in &hashes {
                assert!(
                    gc_store.blob_exists(hash).unwrap(),
                    "retained root {label} was incomplete before GC at {}",
                    hex::encode(hash)
                );
            }
            leased_hashes.extend(hashes);
        }
        let retained_by_pubkey =
            Cid::parse(lease.roots.get("profiles-by-pubkey").unwrap()).unwrap();
        let retained_search = Cid::parse(lease.roots.get("profile-search").unwrap()).unwrap();
        let retained_profile_roots = ProfileIndexRoots {
            by_pubkey: Some(retained_by_pubkey.clone()),
            search: Some(retained_search.clone()),
            by_pubkey_file_sha256: None,
            search_file_sha256: None,
        };

        repair_child.release_boundary(&pause_dir, "after-retention-lease-validated");
        repair_child.wait_for_boundary(&pause_dir, "after-retention-publication-released");
        let gc_report = gc_done_rx
            .recv_timeout(REPAIR_CHILD_TIMEOUT)
            .expect("GC did not enter after validated lease publication")
            .expect("lease-aware generated GC failed");
        gc_thread.join().unwrap();
        assert!(
            gc_report.deleted_dags >= 1,
            "generated GC did not delete its unleased orphan"
        );
        assert!(
            !gc_store.blob_exists(&orphan_hash).unwrap(),
            "unleased generated orphan survived GC"
        );
        for hash in &leased_hashes {
            assert!(
                gc_store.blob_exists(hash).unwrap(),
                "GC deleted lease-protected generated DAG hash {}",
                hex::encode(hash)
            );
        }
        let retained_entry = hashtree_cli::socialgraph::validate_profile_indexes_at_roots(
            gc_store.store_arc(),
            &retained_profile_roots,
            &profile,
        )
        .await
        .expect("read generated encrypted profile indexes after concurrent GC");
        assert_eq!(retained_entry.pubkey, profile.pubkey.to_hex());
        let retained_btree = BTree::new(gc_store.store_arc(), BTreeOptions { order: Some(64) });
        retained_btree
            .validate_link_tree(Some(&retained_by_pubkey))
            .await
            .expect("validate complete retained by-pubkey DAG after GC");
        retained_btree
            .validate_value_tree(Some(&retained_search))
            .await
            .expect("validate complete retained search DAG after GC");
        drop(retained_btree);
        drop(protected_tree);

        repair_child.release_boundary(&pause_dir, "after-retention-publication-released");
        repair_child.wait_for_sigkill("after-durable-intent");
        drop(gc_store);
        assert_eq!(
            hashtree_cli::socialgraph::read_profile_index_roots(&data_dir).unwrap(),
            old_roots,
            "SIGKILL after high-level intent must leave the old pair published"
        );
        assert!(intent_path.is_file());
        assert!(!internal_receipt_path.exists());
        assert!(!receipt_path.exists());
        let (competing_store, competing_graph) = open_writer().unwrap();
        let competing_profile =
            EventBuilder::new(Kind::Metadata, r#"{"name":"must not strand repair"}"#)
                .custom_created_at(Timestamp::from_secs(11))
                .sign_with_keys(&Keys::generate())
                .unwrap();
        let competing_event_root = competing_graph.public_events_root().unwrap();
        let pending_projection_path = data_dir
            .join("socialgraph")
            .join("profile-projection.pending.json");
        let pending_projection_before = read_optional_test_file(&pending_projection_path);
        let competing_error = hashtree_cli::socialgraph::ingest_parsed_event_with_storage_class(
            competing_graph.as_ref(),
            &competing_profile,
            hashtree_cli::socialgraph::EventStorageClass::Public,
        )
        .expect_err("an ordinary root writer must honor the incomplete durable repair");
        assert!(format!("{competing_error:#}").contains("incomplete durable repair intent"));
        assert_eq!(
            competing_graph.public_events_root().unwrap(),
            competing_event_root,
            "blocked ordinary ingest must not advance the event root"
        );
        assert_eq!(
            read_optional_test_file(&pending_projection_path),
            pending_projection_before,
            "blocked ordinary ingest must not create or alter a projection journal"
        );
        drop(competing_graph);
        drop(competing_store);
        assert_eq!(
            hashtree_cli::socialgraph::read_profile_index_roots(&data_dir).unwrap(),
            old_roots,
            "a competing local writer must not strand durable repair recovery"
        );

        run_generated_repair_until_sigkill(&child_context_path, "after-root-pair-commit");
        assert_ne!(
            hashtree_cli::socialgraph::read_profile_index_roots(&data_dir).unwrap(),
            old_roots,
            "the exact new pair must be durable before SIGKILL at the receipt boundary"
        );
        assert!(!internal_receipt_path.exists());
        assert!(!receipt_path.exists());

        run_generated_repair_until_sigkill(&child_context_path, "after-durable-receipt");
        assert!(
            internal_receipt_path.is_file(),
            "receipt must be durable at its explicit SIGKILL boundary"
        );
        assert!(
            !internal_completion_path.exists(),
            "completion witness must remain absent when killed after receipt durability"
        );
        assert!(
            !receipt_path.exists(),
            "external receipt output must wait for the completion witness"
        );

        repair_bulk_projection_profiles(&data_dir, options.clone(), &mut open_writer)
            .await
            .expect("a committed exact pair with a receipt but no completion must be adoptable");
        assert!(
            internal_completion_path.is_file(),
            "receipt recovery must publish the exact completion witness"
        );
        let receipt_before = std::fs::read(&receipt_path).unwrap();
        let (drift_store, drift_graph) = open_writer().unwrap();
        let drift_local = drift_store.router().local_store();
        let hashtree_cli::storage::LocalStore::Pool(drift_pool) = drift_local.as_ref() else {
            panic!("generated repair store did not reopen as PoolStore");
        };
        let drift_data = b"unrelated post-repair PoolStore write";
        let drift_hash: [u8; 32] = Sha256::digest(drift_data).into();
        assert!(drift_pool.put_sync(drift_hash, drift_data).unwrap());
        drift_pool.force_sync().unwrap();
        drop(drift_local);
        drop(drift_graph);
        drop(drift_store);
        let fence_path = hashtree_cli::socialgraph::profile_publication_fence_path(&data_dir);
        std::fs::remove_file(&fence_path).unwrap();
        let opens_before_terminal_retry = writer_opens.get();
        repair_bulk_projection_profiles(&data_dir, options, &mut open_writer)
            .await
            .expect("immutable receipt must remain terminal and restore its publication fence");
        assert_eq!(
            writer_opens.get(),
            opens_before_terminal_retry,
            "terminal receipt retry must not reopen a writer"
        );
        assert_eq!(std::fs::read(&receipt_path).unwrap(), receipt_before);
        assert_eq!(
            std::fs::read(fence_path).unwrap(),
            PROFILE_PUBLICATION_FENCE_BYTES
        );
        let installed = hashtree_cli::socialgraph::read_profile_index_roots(&data_dir).unwrap();

        let read_only =
            Arc::new(ReadOnlyPoolStore::open(data_dir.join(SHARED_BLOB_POOL_DIR_NAME)).unwrap());
        let catalog = read_only.validate_committed_catalog().unwrap();
        assert!(catalog.stored_locations > 0);
        let entry = hashtree_cli::socialgraph::validate_profile_indexes_read_only(
            &data_dir,
            Arc::clone(&read_only),
            &profile,
        )
        .await
        .unwrap();
        assert_eq!(entry.pubkey, profile.pubkey.to_hex());
        let btree = BTree::new(Arc::clone(&read_only), BTreeOptions { order: Some(64) });
        assert!(btree
            .get_link(installed.by_pubkey.as_ref(), &stale.pubkey.to_hex())
            .await
            .unwrap()
            .is_none());
        btree
            .validate_link_tree(installed.by_pubkey.as_ref())
            .await
            .unwrap();
        btree
            .validate_value_tree(installed.search.as_ref())
            .await
            .unwrap();
    }
}
