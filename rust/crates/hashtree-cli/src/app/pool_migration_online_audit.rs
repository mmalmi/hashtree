use anyhow::{bail, Context, Result};
use hashtree_core::types::Hash;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::pool_migration_evidence::{
    validate_root_owned_source_evidence_metadata, validate_source_evidence_metadata,
    SourceEvidenceManifestAuthorityV3, SourceEvidenceManifestReaderV3,
    ONLINE_TARGET_EVIDENCE_FILE_NAME, SOURCE_EVIDENCE_FILE_NAME,
};
use super::pool_migration_protocol::{
    CursorAuthorityV3, FileAuthorityV3, FileIdentityV3, LmdbIdentityV3, NamedFileAuthorityV3,
    WriterUnitMaskV3,
};

pub(super) const ONLINE_TARGET_AUDIT_SCHEMA: &str =
    "hashtree-pool-migration-online-target-audit/v6";
pub(super) const ONLINE_TARGET_AUDIT_FILE_NAME: &str = "online-target-audit.json";
pub(super) const ONLINE_TARGET_AUDIT_CERTIFICATION_SCHEMA: &str =
    "hashtree-pool-migration-online-target-audit-certification/v6";
pub(super) const ONLINE_TARGET_AUDIT_CERTIFICATION_FILE_NAME: &str =
    "online-target-audit-certification.json";
pub(super) const ONLINE_TARGET_AUDIT_CAS_LABEL_PREFIX: &str = "online-target-audit-";
// 4 GiB holds about 107 million exact hash/size records. This covers the
// current Main+Social union while placing a hard bound on root staging work.
// The audit LMDB has a separate 32 GiB virtual map bound for its independent
// source-reconciliation and target-body proof trees.
pub(super) const MAX_REUSABLE_TARGET_EVIDENCE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(super) const SOURCE_EVIDENCE_KIND: &str =
    "source-key-target-catalog-reconciliation/sha256-key-target-size/v2";
pub(super) const TARGET_EVIDENCE_KIND: &str = "target-body/sha256-hash-size/v1";

pub(super) fn online_audit_path(cursor_path: &Path) -> Result<PathBuf> {
    let cursor_name = cursor_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .context("online audit cursor name is not UTF-8")?;
    Ok(cursor_path.with_file_name(format!("{cursor_name}.online-audit-v6")))
}

#[cfg(feature = "lmdb")]
pub(super) fn verify_exact_stored_target_catalog_entries(
    reader: &hashtree_lmdb::PoolStoreReader,
    entries: &[(Hash, u64)],
) -> Result<()> {
    exact_stored_target_locations(reader, entries).map(|_| ())
}

#[cfg(feature = "lmdb")]
pub(super) fn verify_and_record_target_body_page(
    reader: &hashtree_lmdb::PoolStoreReader,
    audit: &hashtree_lmdb::PoolMigrationAuditStore,
    entries: &[(Hash, u64)],
    cursor: Hash,
    byte_limit: u64,
) -> Result<()> {
    if byte_limit == 0 {
        bail!("root target audit buffer limit must be non-zero");
    }
    let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    let locations = exact_stored_target_locations(reader, entries)?;
    let mut offset = 0usize;
    while offset < hashes.len() {
        let bodies = reader
            .read_hashes_bounded(&hashes[offset..], byte_limit)
            .context("root-read online target audit bodies")?;
        if bodies.is_empty() {
            bail!("root target audit body reader made no progress");
        }
        for (body, (expected_hash, expected_size)) in bodies.iter().zip(&entries[offset..]) {
            if body.hash != *expected_hash
                || body.declared_size != Some(*expected_size)
                || body.data.as_ref().map(|data| data.len() as u64) != Some(*expected_size)
                || body.error.is_some()
            {
                bail!("root target audit body differs from checkpoint hash/size authority");
            }
        }
        offset = offset
            .checked_add(bodies.len())
            .context("root target audit offset overflow")?;
    }
    let final_locations = reader
        .blob_catalog_locations(&hashes)
        .context("root-revalidate online target audit catalog")?;
    if final_locations != locations {
        bail!("online target catalog changed during root content verification");
    }
    audit.record_verified_target_page(entries, cursor)?;
    Ok(())
}

#[cfg(feature = "lmdb")]
fn exact_stored_target_locations(
    reader: &hashtree_lmdb::PoolStoreReader,
    entries: &[(Hash, u64)],
) -> Result<Vec<hashtree_lmdb::PoolCatalogLocation>> {
    let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    let locations = reader
        .blob_catalog_locations(&hashes)
        .context("root-read online target audit catalog")?;
    for ((hash, expected_size), location) in entries.iter().zip(&locations) {
        if !matches!(
            location,
            hashtree_lmdb::PoolCatalogLocation::Stored { size, .. }
                if size == expected_size
        ) {
            bail!(
                "root target audit requires exact Stored location for {} / {} bytes",
                hashtree_core::to_hex(hash),
                expected_size
            );
        }
    }
    Ok(locations)
}

pub(super) fn compute_online_audit_binding(
    rollout_id: &str,
    worker_binary_sha256: &str,
    source_baseline_sha256: &str,
    source_lmdb_identity: LmdbIdentityV3,
    source_external_identity: Option<FileIdentityV3>,
    pool_lmdb_identity: LmdbIdentityV3,
    pool_topology_sha256: &str,
    pool_manifest_sha256: Hash,
    prior_target_audit_certification_sha256: Option<&str>,
) -> Result<Hash> {
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-pool-migration-online-audit-authority/v6\0");
    hasher.update(SOURCE_EVIDENCE_KIND.as_bytes());
    hasher.update(b"\0");
    hasher.update(TARGET_EVIDENCE_KIND.as_bytes());
    hasher.update(b"\0");
    hasher.update(rollout_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(worker_binary_sha256.as_bytes());
    hasher.update(b"\0");
    hasher.update(source_baseline_sha256.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        serde_json::to_vec(&source_lmdb_identity)
            .context("serialize online audit source identity")?,
    );
    hasher.update(
        serde_json::to_vec(&source_external_identity)
            .context("serialize online audit source external identity")?,
    );
    hasher.update(
        serde_json::to_vec(&pool_lmdb_identity).context("serialize online audit Pool identity")?,
    );
    hasher.update(pool_topology_sha256.as_bytes());
    hasher.update(pool_manifest_sha256);
    // Preserve the v6 authority exactly when no parent proof is imported so
    // an in-progress pre-import rollout remains resumable with the same
    // binary. Imported proofs add a non-optional parent edge.
    if let Some(parent) = prior_target_audit_certification_sha256 {
        require_sha256("prior target-audit certification", parent)?;
        hasher.update(b"\0prior-target-audit-certification\0");
        hasher.update(parent.as_bytes());
    }
    Ok(hasher.finalize().into())
}

pub(super) fn compute_prior_target_import_authority(
    certification_sha256: &str,
    evidence: &SourceEvidenceManifestAuthorityV3,
) -> Result<Hash> {
    require_sha256("prior target-audit certification", certification_sha256)?;
    require_sha256("prior target evidence", &evidence.sha256)?;
    if evidence.len > MAX_REUSABLE_TARGET_EVIDENCE_BYTES {
        bail!(
            "prior target evidence exceeds the {} byte import bound",
            MAX_REUSABLE_TARGET_EVIDENCE_BYTES
        );
    }
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-pool-migration-target-import-authority/v1\0");
    hasher.update(certification_sha256.as_bytes());
    hasher.update(b"\0");
    hasher
        .update(serde_json::to_vec(evidence).context("serialize prior target evidence authority")?);
    Ok(hasher.finalize().into())
}

pub(super) fn compute_online_target_fence_binding(
    rollout_id: &str,
    writer_units: &[String],
    writer_unit_masks: &[WriterUnitMaskV3],
    legacy_worker_template_mask: &WriterUnitMaskV3,
    legacy_worker_instance_masks: &[WriterUnitMaskV3],
) -> Result<Hash> {
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-pool-migration-online-target-fence/v3\0");
    hasher.update(rollout_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        serde_json::to_vec(&(
            writer_units,
            writer_unit_masks,
            legacy_worker_template_mask,
            legacy_worker_instance_masks,
        ))
        .context("serialize online target fence authority")?,
    );
    Ok(hasher.finalize().into())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolMigrationOnlineTargetAuditReceiptV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) phase: String,
    pub(super) rollout_id: String,
    pub(super) boot_id: String,
    pub(super) attempt_namespace: PathBuf,
    pub(super) attempt_namespace_identity: FileIdentityV3,
    pub(super) attempt_identity: FileIdentityV3,
    pub(super) attempt_nonce: String,
    pub(super) request_path: PathBuf,
    pub(super) request_sha256: String,
    pub(super) acknowledgement_path: PathBuf,
    pub(super) acknowledgement_sha256: String,
    pub(super) terminal_cursor: CursorAuthorityV3,
    pub(super) worker_binary: FileAuthorityV3,
    pub(super) worker_argv_sha256: String,
    pub(super) systemd_unit: String,
    pub(super) systemd_invocation_id: String,
    pub(super) systemd_fragment: FileAuthorityV3,
    pub(super) systemd_environment_file: FileAuthorityV3,
    pub(super) main_pid: u32,
    pub(super) proc_start_time_ticks: u64,
    pub(super) controller_state_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) prior_target_audit_certification_sha256: Option<String>,
    pub(super) source_path: PathBuf,
    pub(super) source_lmdb_identity: LmdbIdentityV3,
    pub(super) source_external_path: Option<PathBuf>,
    pub(super) source_external_identity: Option<FileIdentityV3>,
    pub(super) source_baseline_sha256: String,
    pub(super) pool_path: PathBuf,
    pub(super) pool_lmdb_identity: LmdbIdentityV3,
    pub(super) pool_topology_sha256: String,
    pub(super) pool_manifest_sha256: String,
    pub(super) audit_store_path: PathBuf,
    pub(super) audit_binding_sha256: String,
    pub(super) source_evidence_kind: String,
    pub(super) source_verified_entries: u64,
    pub(super) source_verified_bytes: u64,
    pub(super) source_content_sha256: String,
    pub(super) source_evidence: SourceEvidenceManifestAuthorityV3,
    pub(super) target_evidence_kind: String,
    pub(super) target_verified_entries: u64,
    pub(super) target_verified_bytes: u64,
    pub(super) target_content_sha256: String,
    pub(super) target_evidence: SourceEvidenceManifestAuthorityV3,
    pub(super) target_fence_binding_sha256: String,
    pub(super) target_writer_units: Vec<String>,
    pub(super) target_writer_unit_masks: Vec<WriterUnitMaskV3>,
    pub(super) legacy_worker_template_mask: WriterUnitMaskV3,
    pub(super) legacy_worker_instance_masks: Vec<WriterUnitMaskV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolMigrationOnlineTargetAuditCertificationV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) rollout_id: String,
    pub(super) controller_state_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) prior_target_audit_certification_sha256: Option<String>,
    pub(super) receipt: FileAuthorityV3,
    pub(super) source_evidence: SourceEvidenceManifestAuthorityV3,
    pub(super) target_evidence: SourceEvidenceManifestAuthorityV3,
    #[serde(default)]
    pub(super) evidence_root_owned: bool,
    pub(super) certified_at_unix_seconds: u64,
}

pub(super) struct OnlineTargetAuditExpectationV3<'a> {
    pub(super) rollout_id: &'a str,
    pub(super) worker_binary_sha256: &'a str,
    pub(super) source_baseline_sha256: &'a str,
    pub(super) source_path: &'a Path,
    pub(super) source_lmdb_identity: LmdbIdentityV3,
    pub(super) source_external_path: Option<&'a Path>,
    pub(super) source_external_identity: Option<FileIdentityV3>,
    pub(super) pool_path: &'a Path,
    pub(super) pool_lmdb_identity: LmdbIdentityV3,
    pub(super) pool_topology_sha256: &'a str,
    pub(super) pool_manifest_sha256: &'a str,
    pub(super) target_writer_units: &'a [String],
    pub(super) target_writer_unit_masks: &'a [WriterUnitMaskV3],
    pub(super) legacy_worker_template_mask: &'a WriterUnitMaskV3,
    pub(super) legacy_worker_instance_masks: &'a [WriterUnitMaskV3],
    pub(super) expected_service_gid: u32,
    pub(super) validate_evidence_content: bool,
}

pub(super) struct PriorOnlineTargetAuditExpectationV3<'a> {
    pub(super) boot_id: &'a str,
    pub(super) pool_path: &'a Path,
    pub(super) pool_lmdb_identity: LmdbIdentityV3,
    pub(super) pool_topology_sha256: &'a str,
    pub(super) pool_manifest_sha256: &'a str,
    pub(super) expected_service_gid: u32,
    pub(super) validate_evidence_content: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ValidatedOnlineTargetAuditV3 {
    pub(super) certification_sha256: String,
    pub(super) receipt: PoolMigrationOnlineTargetAuditReceiptV3,
}

pub(super) fn load_validated_online_target_audit(
    authorities: &[NamedFileAuthorityV3],
    expected: &OnlineTargetAuditExpectationV3<'_>,
) -> Result<Option<ValidatedOnlineTargetAuditV3>> {
    let Some(authority) = matching_online_target_audit_authority(authorities)? else {
        return Ok(None);
    };
    let (_, receipt) =
        read_certified_online_target_audit(authority, expected.expected_service_gid)?;
    if receipt.rollout_id != expected.rollout_id {
        bail!("online target audit certification has invalid release authority");
    }
    if receipt.source_path != expected.source_path
        || receipt.source_evidence_kind != SOURCE_EVIDENCE_KIND
        || receipt.target_evidence_kind != TARGET_EVIDENCE_KIND
        || receipt.worker_binary.sha256 != expected.worker_binary_sha256
        || receipt.source_baseline_sha256 != expected.source_baseline_sha256
        || receipt.source_lmdb_identity != expected.source_lmdb_identity
        || receipt.source_external_path.as_deref() != expected.source_external_path
        || receipt.source_external_identity != expected.source_external_identity
        || receipt.pool_path != expected.pool_path
        || receipt.pool_lmdb_identity != expected.pool_lmdb_identity
        || receipt.pool_topology_sha256 != expected.pool_topology_sha256
        || receipt.pool_manifest_sha256 != expected.pool_manifest_sha256
        || receipt.target_writer_units != expected.target_writer_units
        || receipt.target_writer_unit_masks != expected.target_writer_unit_masks
        || receipt.legacy_worker_template_mask != *expected.legacy_worker_template_mask
        || receipt.legacy_worker_instance_masks != expected.legacy_worker_instance_masks
    {
        bail!("online target audit does not bind the exact source and Pool authority");
    }
    let audit_binding = compute_online_audit_binding(
        expected.rollout_id,
        expected.worker_binary_sha256,
        expected.source_baseline_sha256,
        expected.source_lmdb_identity,
        expected.source_external_identity,
        expected.pool_lmdb_identity,
        expected.pool_topology_sha256,
        hashtree_core::from_hex(expected.pool_manifest_sha256)
            .context("decode online target audit Pool manifest")?,
        receipt.prior_target_audit_certification_sha256.as_deref(),
    )?;
    if receipt.audit_binding_sha256 != hashtree_core::to_hex(&audit_binding) {
        bail!("online target audit receipt has an invalid audit binding");
    }
    let target_fence_binding = compute_online_target_fence_binding(
        expected.rollout_id,
        expected.target_writer_units,
        expected.target_writer_unit_masks,
        expected.legacy_worker_template_mask,
        expected.legacy_worker_instance_masks,
    )?;
    if receipt.target_fence_binding_sha256 != hashtree_core::to_hex(&target_fence_binding) {
        bail!("online target audit receipt has an invalid target fence binding");
    }
    validate_source_evidence_metadata(
        &receipt.source_evidence,
        Some(expected.expected_service_gid),
        expected.validate_evidence_content,
    )?;
    validate_source_evidence_metadata(
        &receipt.target_evidence,
        Some(expected.expected_service_gid),
        expected.validate_evidence_content,
    )?;
    if expected.validate_evidence_content {
        let mut source_evidence = SourceEvidenceManifestReaderV3::open(&receipt.source_evidence)?;
        while source_evidence.next_entry()?.is_some() {}
        let source_summary = source_evidence.validated_summary()?;
        if source_summary.entries != receipt.source_verified_entries
            || source_summary.bytes != receipt.source_verified_bytes
            || hashtree_core::to_hex(&source_summary.content_sha256)
                != receipt.source_content_sha256
        {
            bail!("online source evidence differs from its receipt summary");
        }
        let mut target_evidence = SourceEvidenceManifestReaderV3::open(&receipt.target_evidence)?;
        while target_evidence.next_entry()?.is_some() {}
        let target_summary = target_evidence.validated_summary()?;
        if target_summary.entries != receipt.target_verified_entries
            || target_summary.bytes != receipt.target_verified_bytes
            || hashtree_core::to_hex(&target_summary.content_sha256)
                != receipt.target_content_sha256
        {
            bail!("online target evidence differs from its receipt summary");
        }
    }
    Ok(Some(ValidatedOnlineTargetAuditV3 {
        certification_sha256: authority.sha256.clone(),
        receipt,
    }))
}

/// Load one root-certified target-body proof from an earlier rollout that
/// addressed the exact same physical Pool authority.
///
/// Source and worker identities may differ. The imported evidence is still
/// only a hash/size body proof: the new rollout must rescan every current
/// catalog row, and can reuse only exact `Stored` hash/size matches.
pub(super) fn load_validated_prior_online_target_audit(
    authorities: &[NamedFileAuthorityV3],
    expected: &PriorOnlineTargetAuditExpectationV3<'_>,
) -> Result<Option<ValidatedOnlineTargetAuditV3>> {
    let Some(authority) = matching_online_target_audit_authority(authorities)? else {
        return Ok(None);
    };
    let (_, receipt) =
        read_certified_online_target_audit(authority, expected.expected_service_gid)?;
    if receipt.boot_id != expected.boot_id
        || receipt.source_evidence_kind != SOURCE_EVIDENCE_KIND
        || receipt.target_evidence_kind != TARGET_EVIDENCE_KIND
        || receipt.pool_path != expected.pool_path
        || receipt.pool_lmdb_identity != expected.pool_lmdb_identity
        || receipt.pool_topology_sha256 != expected.pool_topology_sha256
        || receipt.pool_manifest_sha256 != expected.pool_manifest_sha256
    {
        bail!("prior target audit does not bind the exact current Pool authority");
    }
    if receipt.target_evidence.len > MAX_REUSABLE_TARGET_EVIDENCE_BYTES {
        bail!(
            "prior target evidence exceeds the {} byte import bound",
            MAX_REUSABLE_TARGET_EVIDENCE_BYTES
        );
    }
    let audit_binding = compute_online_audit_binding(
        &receipt.rollout_id,
        &receipt.worker_binary.sha256,
        &receipt.source_baseline_sha256,
        receipt.source_lmdb_identity,
        receipt.source_external_identity,
        expected.pool_lmdb_identity,
        expected.pool_topology_sha256,
        hashtree_core::from_hex(expected.pool_manifest_sha256)
            .context("decode prior target audit Pool manifest")?,
        receipt.prior_target_audit_certification_sha256.as_deref(),
    )?;
    if receipt.audit_binding_sha256 != hashtree_core::to_hex(&audit_binding) {
        bail!("prior target audit receipt has an invalid audit binding");
    }
    let target_fence_binding = compute_online_target_fence_binding(
        &receipt.rollout_id,
        &receipt.target_writer_units,
        &receipt.target_writer_unit_masks,
        &receipt.legacy_worker_template_mask,
        &receipt.legacy_worker_instance_masks,
    )?;
    if receipt.target_writer_units.is_empty()
        || receipt.target_fence_binding_sha256 != hashtree_core::to_hex(&target_fence_binding)
    {
        bail!("prior target audit receipt has an invalid target fence binding");
    }
    validate_source_evidence_metadata(
        &receipt.target_evidence,
        Some(expected.expected_service_gid),
        false,
    )?;
    if expected.validate_evidence_content {
        let mut target_evidence = SourceEvidenceManifestReaderV3::open(&receipt.target_evidence)?;
        while target_evidence.next_entry()?.is_some() {}
        let target_summary = target_evidence.validated_summary()?;
        if target_summary.entries != receipt.target_verified_entries
            || target_summary.bytes != receipt.target_verified_bytes
            || hashtree_core::to_hex(&target_summary.content_sha256)
                != receipt.target_content_sha256
        {
            bail!("prior online target evidence differs from its receipt summary");
        }
    }
    Ok(Some(ValidatedOnlineTargetAuditV3 {
        certification_sha256: authority.sha256.clone(),
        receipt,
    }))
}

fn matching_online_target_audit_authority(
    authorities: &[NamedFileAuthorityV3],
) -> Result<Option<&NamedFileAuthorityV3>> {
    let matching = authorities
        .iter()
        .filter(|authority| {
            authority
                .label
                .starts_with(ONLINE_TARGET_AUDIT_CAS_LABEL_PREFIX)
        })
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        bail!("exactly one online target audit certification may be supplied");
    }
    Ok(matching.first().copied())
}

fn read_certified_online_target_audit(
    authority: &NamedFileAuthorityV3,
    expected_service_gid: u32,
) -> Result<(
    PoolMigrationOnlineTargetAuditCertificationV3,
    PoolMigrationOnlineTargetAuditReceiptV3,
)> {
    require_sha256("online target audit certification", &authority.sha256)?;
    let certification_bytes = read_bounded_regular(
        &authority.path,
        1024 * 1024,
        Some((0, expected_service_gid, 0o440)),
        "online target audit certification",
    )?;
    if sha256_bytes(&certification_bytes) != authority.sha256 {
        bail!("online target audit certification differs from its CAS digest");
    }
    let certification: PoolMigrationOnlineTargetAuditCertificationV3 =
        serde_json::from_slice(&certification_bytes)
            .context("parse strict online target audit certification")?;
    if certification.schema != ONLINE_TARGET_AUDIT_CERTIFICATION_SCHEMA
        || certification.status != "certified"
    {
        bail!("online target audit certification has invalid release authority");
    }
    require_sha256(
        "online target audit controller state",
        &certification.controller_state_sha256,
    )?;
    require_sha256("online target audit receipt", &certification.receipt.sha256)?;
    let receipt_bytes = read_bounded_regular(
        &certification.receipt.path,
        1024 * 1024,
        Some((u32::MAX, expected_service_gid, 0o640)),
        "online target audit receipt",
    )?;
    if sha256_bytes(&receipt_bytes) != certification.receipt.sha256 {
        bail!("online target audit receipt differs from its certification digest");
    }
    let receipt: PoolMigrationOnlineTargetAuditReceiptV3 =
        serde_json::from_slice(&receipt_bytes)
            .context("parse strict online target audit receipt")?;
    if receipt.schema != ONLINE_TARGET_AUDIT_SCHEMA
        || receipt.status != "verified"
        || receipt.phase != "online-bounded"
        || receipt.rollout_id != certification.rollout_id
        || receipt.controller_state_sha256 != certification.controller_state_sha256
        || receipt.prior_target_audit_certification_sha256
            != certification.prior_target_audit_certification_sha256
    {
        bail!("online target audit receipt has invalid release bindings");
    }
    if authority.label
        != format!(
            "{ONLINE_TARGET_AUDIT_CAS_LABEL_PREFIX}{}",
            receipt.attempt_nonce
        )
    {
        bail!("online target audit CAS label must end in the exact attempt nonce");
    }
    for (label, value) in [
        ("request", receipt.request_sha256.as_str()),
        ("acknowledgement", receipt.acknowledgement_sha256.as_str()),
        ("worker binary", receipt.worker_binary.sha256.as_str()),
        ("worker argv", receipt.worker_argv_sha256.as_str()),
        ("systemd fragment", receipt.systemd_fragment.sha256.as_str()),
        (
            "systemd environment",
            receipt.systemd_environment_file.sha256.as_str(),
        ),
        ("source baseline", receipt.source_baseline_sha256.as_str()),
        ("Pool topology", receipt.pool_topology_sha256.as_str()),
        ("Pool manifest", receipt.pool_manifest_sha256.as_str()),
        ("audit binding", receipt.audit_binding_sha256.as_str()),
        ("source content", receipt.source_content_sha256.as_str()),
        ("target content", receipt.target_content_sha256.as_str()),
        (
            "target fence binding",
            receipt.target_fence_binding_sha256.as_str(),
        ),
    ] {
        require_sha256(label, value)?;
    }
    let terminal_cursor_shape_valid = if receipt.terminal_cursor.exists {
        receipt.terminal_cursor.value.is_some() && receipt.terminal_cursor.sha256.is_some()
    } else {
        receipt.terminal_cursor.value.is_none() && receipt.terminal_cursor.sha256.is_none()
    };
    if receipt.main_pid == 0
        || receipt.proc_start_time_ticks == 0
        || !terminal_cursor_shape_valid
        || receipt.source_evidence != certification.source_evidence
        || receipt.target_evidence != certification.target_evidence
        || receipt.source_evidence.entries != receipt.source_verified_entries
        || receipt.target_evidence.entries != receipt.target_verified_entries
        || receipt.source_evidence.path
            != receipt
                .request_path
                .parent()
                .context("online target audit request path has no attempt directory")?
                .join(SOURCE_EVIDENCE_FILE_NAME)
        || receipt.target_evidence.path
            != receipt
                .request_path
                .parent()
                .context("online target audit request path has no attempt directory")?
                .join(ONLINE_TARGET_EVIDENCE_FILE_NAME)
    {
        bail!("online target audit receipt has an incomplete terminal authority");
    }
    if certification.evidence_root_owned {
        validate_root_owned_source_evidence_metadata(
            &certification.source_evidence,
            expected_service_gid,
        )?;
        validate_root_owned_source_evidence_metadata(
            &certification.target_evidence,
            expected_service_gid,
        )?;
    }
    Ok((certification, receipt))
}

fn read_bounded_regular(
    path: &Path,
    max_bytes: u64,
    unix_authority: Option<(u32, u32, u32)>,
    label: &str,
) -> Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("open {label}"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label}"))?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        bail!("{label} is not a bounded regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some((uid, gid, mode)) = unix_authority {
            if (uid != u32::MAX && metadata.uid() != uid)
                || metadata.gid() != gid
                || metadata.mode() & 0o7777 != mode
                || metadata.nlink() != 1
            {
                bail!("{label} ownership/mode differs from its authority");
            }
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    let read_limit = max_bytes
        .checked_add(1)
        .context("bounded audit read limit overflow")?;
    file.by_ref()
        .take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label}"))?;
    if bytes.len() as u64 > max_bytes {
        bail!("{label} exceeded its hard read bound");
    }
    Ok(bytes)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn require_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be exactly 64 lowercase hexadecimal characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lmdb_identity(seed: u64) -> LmdbIdentityV3 {
        LmdbIdentityV3 {
            directory: FileIdentityV3 {
                device: seed,
                inode: seed + 1,
            },
            data: FileIdentityV3 {
                device: seed,
                inode: seed + 2,
            },
            lock: FileIdentityV3 {
                device: seed,
                inode: seed + 3,
            },
        }
    }

    #[test]
    fn audit_binding_rejects_a_different_worker_semantics_binary() {
        let binding = |binary: &str| {
            compute_online_audit_binding(
                "rollout",
                binary,
                &"22".repeat(32),
                lmdb_identity(10),
                Some(FileIdentityV3 {
                    device: 20,
                    inode: 21,
                }),
                lmdb_identity(30),
                &"33".repeat(32),
                [0x44; 32],
                None,
            )
            .expect("compute binding")
        };

        assert_ne!(binding(&"00".repeat(32)), binding(&"11".repeat(32)));
    }

    #[test]
    fn audit_binding_adds_an_exact_parent_certification_edge() {
        let binding = |parent: Option<&str>| {
            compute_online_audit_binding(
                "rollout",
                &"11".repeat(32),
                &"22".repeat(32),
                lmdb_identity(10),
                None,
                lmdb_identity(30),
                &"33".repeat(32),
                [0x44; 32],
                parent,
            )
            .expect("compute binding")
        };
        let first = "55".repeat(32);
        let second = "66".repeat(32);
        assert_ne!(binding(None), binding(Some(&first)));
        assert_ne!(binding(Some(&first)), binding(Some(&second)));
    }

    #[test]
    fn reusable_target_evidence_has_an_explicit_four_gibibyte_bound() {
        let mut evidence = SourceEvidenceManifestAuthorityV3 {
            path: PathBuf::from("/generated/online-target-hash-size.manifest"),
            parent_identity: FileIdentityV3 {
                device: 1,
                inode: 2,
            },
            identity: FileIdentityV3 {
                device: 1,
                inode: 3,
            },
            len: MAX_REUSABLE_TARGET_EVIDENCE_BYTES,
            entries: 0,
            sha256: "77".repeat(32),
        };
        compute_prior_target_import_authority(&"88".repeat(32), &evidence)
            .expect("accept exact reusable evidence bound");
        evidence.len += 1;
        assert!(
            compute_prior_target_import_authority(&"88".repeat(32), &evidence)
                .expect_err("reject evidence beyond reusable bound")
                .to_string()
                .contains("exceeds")
        );
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn corrupt_exact_size_stored_target_cannot_enter_body_proof_ledger() {
        use hashtree_lmdb::{
            LmdbBlobStore, PoolMemberConfig, PoolMigrationAuditStore, PoolStore, PoolStoreConfig,
            PoolStoreReader,
        };

        let temp = tempfile::tempdir().expect("temporary real Pool");
        let catalog = temp.path().join("catalog");
        let member = temp.path().join("member");
        let pool =
            PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open real target Pool");
        pool.add_member(PoolMemberConfig::new(member.clone(), 1024 * 1024))
            .expect("add real target member");
        let body = b"target body proof must hash physical bytes".repeat(32);
        let hash = hashtree_core::sha256(&body);
        assert!(pool.put_sync(hash, &body).expect("store valid target body"));
        drop(pool);

        let raw_member = LmdbBlobStore::with_exact_map_size_and_external_blob_options(
            &member,
            16 * 1024 * 1024,
            None,
        )
        .expect("open physical target member");
        assert!(raw_member
            .delete_sync(&hash)
            .expect("delete valid physical body"));
        let corrupt = vec![0xa5; body.len()];
        assert_eq!(corrupt.len(), body.len());
        assert!(raw_member
            .put_sync(hash, &corrupt)
            .expect("write equal-size corrupt bytes under the catalogued hash"));
        raw_member.force_sync().expect("sync corrupt physical body");
        drop(raw_member);

        let mut reader_config = PoolStoreConfig::default();
        reader_config.temperature.enabled = false;
        let reader =
            PoolStoreReader::open(&catalog, reader_config).expect("open real target reader");
        let entries = [(hash, body.len() as u64)];
        verify_exact_stored_target_catalog_entries(&reader, &entries)
            .expect("catalog metadata alone still looks exactly Stored");

        let audit = PoolMigrationAuditStore::open(
            &temp.path().join("audit"),
            hashtree_core::sha256(b"negative target body proof authority"),
        )
        .expect("open real root-owned proof ledger");
        let error = verify_and_record_target_body_page(&reader, &audit, &entries, hash, u64::MAX)
            .expect_err("equal-size corrupt target bytes must not become a body proof");
        assert!(error
            .to_string()
            .contains("body differs from checkpoint hash/size authority"));
        assert_eq!(
            audit
                .contains_target_exact_sorted(&entries)
                .expect("query target proof ledger"),
            vec![false]
        );
        assert_eq!(
            audit.target_cursor().expect("query target proof cursor"),
            None,
            "a failed physical proof must not advance the durable target cursor"
        );
    }
}
