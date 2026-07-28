use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use super::pool_migration_evidence::{
    validate_source_evidence_metadata, SourceEvidenceManifestAuthorityV3, SOURCE_EVIDENCE_FILE_NAME,
};
use super::pool_migration_launch::{
    CursorAuthorityV3, FileAuthorityV3, FileIdentityV3, LmdbIdentityV3, NamedFileAuthorityV3,
    PoolMigrationLaunchRequestV3, PoolTopologyV3, WriterUnitMaskV3, ACK_SCHEMA,
    ATTEMPT_NAMESPACE_NAME, REQUEST_FILE_NAME, REQUEST_SCHEMA,
};
use super::pool_migration_mount::{
    validate_source_read_only_mount_authority, SourceReadOnlyMountAuthorityV3,
};
use super::pool_migration_terminal_publication::read_validated_source_terminal_certification;
use hashtree_lmdb::LmdbEnvironmentGeneration;

pub(super) const SOURCE_TERMINAL_FILE_NAME: &str = "source-terminal.json";
pub(super) const SOURCE_TERMINAL_SCHEMA: &str = "hashtree-pool-migration-source-terminal/v3";
pub(super) const SOURCE_TERMINAL_CAS_LABEL_PREFIX: &str = "source-terminal-";
pub(super) const MAX_FINAL_SOURCE_RECEIPTS: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FileMutationSnapshotV3 {
    pub(super) identity: FileIdentityV3,
    pub(super) len: u64,
    pub(super) modified_seconds: i64,
    pub(super) modified_nanoseconds: i64,
    pub(super) changed_seconds: i64,
    pub(super) changed_nanoseconds: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LmdbGenerationV3 {
    pub(super) map_size: u64,
    pub(super) last_page_number: u64,
    pub(super) last_txn_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ExternalCorpusFingerprintV3 {
    pub(super) root: FileMutationSnapshotV3,
    pub(super) directory_entries: u64,
    pub(super) regular_file_entries: u64,
    pub(super) regular_file_bytes: u64,
    pub(super) metadata_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceGenerationFingerprintV3 {
    pub(super) directory: FileMutationSnapshotV3,
    pub(super) data: FileMutationSnapshotV3,
    pub(super) lock_identity: FileIdentityV3,
    pub(super) lmdb: LmdbGenerationV3,
    pub(super) external: Option<ExternalCorpusFingerprintV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolMigrationSourceTerminalReceiptV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) phase: String,
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
    pub(super) source_path: PathBuf,
    pub(super) source_lmdb_identity: LmdbIdentityV3,
    pub(super) source_external_path: Option<PathBuf>,
    pub(super) source_external_identity: Option<FileIdentityV3>,
    pub(super) source_read_only_mounts: SourceReadOnlyMountAuthorityV3,
    pub(super) source_baseline_sha256: String,
    pub(super) source_blob_entries: u64,
    pub(super) source_metadata_entries: u64,
    pub(super) source_blob_only_entries: u64,
    pub(super) source_legacy_blob_only: bool,
    pub(super) source_inline_entries: u64,
    pub(super) source_loose_external_entries: u64,
    pub(super) source_packed_external_entries: u64,
    pub(super) source_keyset_sha256: String,
    pub(super) source_catalog_location_sha256: String,
    pub(super) source_verified_entries: u64,
    pub(super) source_verified_bytes: u64,
    pub(super) source_content_sha256: String,
    pub(super) source_evidence: SourceEvidenceManifestAuthorityV3,
    pub(super) source_generation: SourceGenerationFingerprintV3,
    pub(super) pool_path: PathBuf,
    pub(super) pool_lmdb_identity: LmdbIdentityV3,
    pub(super) pool_topology_sha256: String,
    pub(super) pool_manifest_sha256: String,
    pub(super) pool_topology: PoolTopologyV3,
    pub(super) stopped_writer_units: Vec<String>,
    pub(super) writer_unit_masks: Vec<WriterUnitMaskV3>,
    pub(super) legacy_worker_template_mask: WriterUnitMaskV3,
    pub(super) legacy_worker_instance_masks: Vec<WriterUnitMaskV3>,
    pub(super) source_read_only: bool,
    pub(super) target_audit_deferred: bool,
}

pub(super) struct SourceContentAuditV3 {
    pub(super) verified_entries: u64,
    pub(super) verified_bytes: u64,
    pub(super) sha256: [u8; 32],
}

pub(super) struct PriorSourceReceiptExpectationV3<'a> {
    pub(super) boot_id: &'a str,
    pub(super) pool_path: &'a Path,
    pub(super) pool_lmdb_identity: LmdbIdentityV3,
    pub(super) pool_topology_sha256: &'a str,
    pub(super) pool_manifest_sha256: &'a str,
    pub(super) pool_topology: &'a PoolTopologyV3,
    pub(super) stopped_writer_units: &'a [String],
    pub(super) writer_unit_masks: &'a [WriterUnitMaskV3],
    pub(super) legacy_worker_template_mask: &'a WriterUnitMaskV3,
    pub(super) legacy_worker_instance_masks: &'a [WriterUnitMaskV3],
    pub(super) expected_service_gid: Option<u32>,
    pub(super) validate_physical_generation: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ValidatedSourceTerminalReceiptV3 {
    pub(super) authority_sha256: String,
    pub(super) receipt: PoolMigrationSourceTerminalReceiptV3,
}

pub(super) fn validate_frozen_source_generation(
    receipt: &PoolMigrationSourceTerminalReceiptV3,
    source_runtime_path: &Path,
    source_external_runtime_path: Option<&Path>,
) -> Result<()> {
    let directory = snapshot_retained_path(
        source_runtime_path,
        receipt.source_lmdb_identity.directory,
        ExpectedPathType::Directory,
        "frozen source LMDB directory",
    )?;
    if directory != receipt.source_generation.directory {
        bail!("frozen source directory generation differs from its terminal receipt");
    }
    let data = snapshot_retained_path(
        &source_runtime_path.join("data.mdb"),
        receipt.source_lmdb_identity.data,
        ExpectedPathType::Regular,
        "frozen source LMDB data.mdb",
    )?;
    if data != receipt.source_generation.data {
        bail!("frozen source data generation differs from its terminal receipt");
    }
    let external = source_external_runtime_path.map(|path| hashtree_lmdb::ExternalBlobOptions {
        base_path: path.to_path_buf(),
        min_bytes: 1,
        sync: true,
        pack_target_bytes: None,
    });
    let reader =
        hashtree_lmdb::LmdbBlobReader::open_with_external_read_concurrency_and_pinned_identity(
            source_runtime_path,
            external,
            1,
            hashtree_lmdb::PinnedLmdbIdentity {
                data: hashtree_lmdb::PinnedLmdbFileIdentity {
                    device: receipt.source_lmdb_identity.data.device,
                    inode: receipt.source_lmdb_identity.data.inode,
                },
                lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                    device: receipt.source_lmdb_identity.lock.device,
                    inode: receipt.source_lmdb_identity.lock.inode,
                },
            },
        )
        .context("open frozen source LMDB for cheap generation validation")?;
    let generation = reader.environment_generation();
    drop(reader);
    if (LmdbGenerationV3 {
        map_size: generation.map_size,
        last_page_number: generation.last_page_number,
        last_txn_id: generation.last_txn_id,
    }) != receipt.source_generation.lmdb
    {
        bail!("frozen source LMDB generation differs from its terminal receipt");
    }
    match (
        receipt.source_external_path.as_ref(),
        receipt.source_external_identity,
        receipt.source_generation.external.as_ref(),
        source_external_runtime_path,
    ) {
        (Some(_), Some(identity), Some(expected), Some(runtime_path)) => {
            let current = capture_external_corpus(runtime_path, identity, true)?;
            if &current != expected {
                bail!(
                    "frozen source external corpus fingerprint differs from its terminal receipt"
                );
            }
        }
        (None, None, None, None) => {}
        _ => bail!("frozen source external generation authority is inconsistent"),
    }
    Ok(())
}

pub(super) fn capture_source_generation_fingerprint(
    source_path: &Path,
    source_identity: LmdbIdentityV3,
    source_external_path: Option<&Path>,
    source_external_identity: Option<FileIdentityV3>,
    lmdb: LmdbEnvironmentGeneration,
) -> Result<SourceGenerationFingerprintV3> {
    let directory = snapshot_path(
        source_path,
        source_identity.directory,
        ExpectedPathType::Directory,
        "source LMDB directory",
    )?;
    let data = snapshot_path(
        &source_path.join("data.mdb"),
        source_identity.data,
        ExpectedPathType::Regular,
        "source LMDB data.mdb",
    )?;
    let lock = snapshot_path(
        &source_path.join("lock.mdb"),
        source_identity.lock,
        ExpectedPathType::Regular,
        "source LMDB lock.mdb",
    )?;
    let external = match (source_external_path, source_external_identity) {
        (Some(path), Some(identity)) => Some(capture_external_corpus(path, identity, false)?),
        (None, None) => None,
        _ => bail!("source external path and identity must be present or absent together"),
    };
    Ok(SourceGenerationFingerprintV3 {
        directory,
        data,
        lock_identity: lock.identity,
        lmdb: LmdbGenerationV3 {
            map_size: lmdb.map_size,
            last_page_number: lmdb.last_page_number,
            last_txn_id: lmdb.last_txn_id,
        },
        external,
    })
}

fn capture_external_corpus(
    root: &Path,
    expected_identity: FileIdentityV3,
    allow_retained_procfd: bool,
) -> Result<ExternalCorpusFingerprintV3> {
    let root_snapshot = snapshot_path_impl(
        root,
        expected_identity,
        ExpectedPathType::Directory,
        "source external corpus root",
        allow_retained_procfd,
    )?;
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-pool-migration-source-external-metadata/v3\0");
    let mut directory_entries = 0u64;
    let mut regular_file_entries = 0u64;
    let mut regular_file_bytes = 0u64;
    walk_external_corpus(
        root,
        Path::new(""),
        &mut hasher,
        &mut directory_entries,
        &mut regular_file_entries,
        &mut regular_file_bytes,
        allow_retained_procfd,
    )?;
    let root_after = snapshot_path_impl(
        root,
        expected_identity,
        ExpectedPathType::Directory,
        "source external corpus root",
        allow_retained_procfd,
    )?;
    if root_after != root_snapshot {
        bail!("source external corpus root changed during metadata fingerprinting");
    }
    Ok(ExternalCorpusFingerprintV3 {
        root: root_snapshot,
        directory_entries,
        regular_file_entries,
        regular_file_bytes,
        metadata_sha256: hex::encode(hasher.finalize()),
    })
}

fn walk_external_corpus(
    root: &Path,
    relative_directory: &Path,
    hasher: &mut Sha256,
    directory_entries: &mut u64,
    regular_file_entries: &mut u64,
    regular_file_bytes: &mut u64,
    allow_retained_procfd: bool,
) -> Result<()> {
    let directory_path = root.join(relative_directory);
    let before = external_entry_metadata(
        root,
        relative_directory,
        &directory_path,
        allow_retained_procfd,
    )
    .with_context(|| format!("inspect external directory {}", directory_path.display()))?;
    if !before.file_type().is_dir() {
        bail!(
            "source external corpus contains a non-directory traversal root {}",
            directory_path.display()
        );
    }
    let mut entries = std::fs::read_dir(&directory_path)
        .with_context(|| format!("read external directory {}", directory_path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("enumerate external directory {}", directory_path.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let relative = relative_directory.join(&name);
        let path = root.join(&relative);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect source external entry {}", path.display()))?;
        let kind = if metadata.file_type().is_dir() {
            b'd'
        } else if metadata.file_type().is_file() {
            b'f'
        } else {
            bail!(
                "source external corpus contains a symlink or special entry {}",
                path.display()
            );
        };
        update_external_metadata_digest(hasher, &relative, kind, &metadata);
        if kind == b'd' {
            *directory_entries = directory_entries
                .checked_add(1)
                .context("source external directory count overflow")?;
            walk_external_corpus(
                root,
                &relative,
                hasher,
                directory_entries,
                regular_file_entries,
                regular_file_bytes,
                allow_retained_procfd,
            )?;
        } else {
            #[cfg(unix)]
            if metadata.nlink() != 1 {
                bail!(
                    "source external regular file must be single-link before and throughout its read-only fence: {}",
                    path.display()
                );
            }
            *regular_file_entries = regular_file_entries
                .checked_add(1)
                .context("source external file count overflow")?;
            *regular_file_bytes = regular_file_bytes
                .checked_add(metadata.len())
                .context("source external byte count overflow")?;
            let after = std::fs::symlink_metadata(&path)
                .with_context(|| format!("reinspect source external file {}", path.display()))?;
            if !same_metadata(&metadata, &after) {
                bail!(
                    "source external file changed during metadata fingerprinting: {}",
                    path.display()
                );
            }
        }
    }
    let after = external_entry_metadata(
        root,
        relative_directory,
        &directory_path,
        allow_retained_procfd,
    )
    .with_context(|| format!("reinspect external directory {}", directory_path.display()))?;
    if !same_metadata(&before, &after) {
        bail!(
            "source external directory changed during metadata fingerprinting: {}",
            directory_path.display()
        );
    }
    Ok(())
}

fn external_entry_metadata(
    root: &Path,
    relative: &Path,
    path: &Path,
    allow_retained_procfd: bool,
) -> std::io::Result<std::fs::Metadata> {
    if allow_retained_procfd && relative.as_os_str().is_empty() && is_exact_proc_self_fd_path(root)
    {
        // /proc/self/fd/N is itself a symlink, but N is a controller-retained
        // directory descriptor. Follow that one trusted indirection for the
        // root; descendants continue to use symlink_metadata so corpus
        // symlinks remain forbidden.
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
}

fn update_external_metadata_digest(
    hasher: &mut Sha256,
    relative: &Path,
    kind: u8,
    metadata: &std::fs::Metadata,
) {
    #[cfg(unix)]
    let path_bytes = {
        use std::os::unix::ffi::OsStrExt;
        relative.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let path_storage = relative.to_string_lossy();
    #[cfg(not(unix))]
    let path_bytes = path_storage.as_bytes();
    hasher.update((path_bytes.len() as u64).to_be_bytes());
    hasher.update(path_bytes);
    hasher.update([kind]);
    #[cfg(unix)]
    {
        hasher.update(metadata.dev().to_be_bytes());
        hasher.update(metadata.ino().to_be_bytes());
        hasher.update(metadata.mode().to_be_bytes());
        hasher.update(metadata.nlink().to_be_bytes());
        hasher.update(metadata.len().to_be_bytes());
        hasher.update(metadata.mtime().to_be_bytes());
        hasher.update(metadata.mtime_nsec().to_be_bytes());
        hasher.update(metadata.ctime().to_be_bytes());
        hasher.update(metadata.ctime_nsec().to_be_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(metadata.len().to_be_bytes());
}

fn same_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.mode() == right.mode()
            && left.nlink() == right.nlink()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len()
    }
}

pub(super) fn validate_prior_source_terminal_receipts(
    authorities: &[NamedFileAuthorityV3],
    expected: &PriorSourceReceiptExpectationV3<'_>,
) -> Result<Vec<String>> {
    Ok(
        load_validated_prior_source_terminal_receipts(authorities, expected)?
            .into_iter()
            .map(|validated| validated.authority_sha256)
            .collect(),
    )
}

pub(super) fn load_validated_prior_source_terminal_receipts(
    authorities: &[NamedFileAuthorityV3],
    expected: &PriorSourceReceiptExpectationV3<'_>,
) -> Result<Vec<ValidatedSourceTerminalReceiptV3>> {
    let receipts = bounded_source_terminal_receipt_authorities(authorities)?;
    let mut source_identities = std::collections::HashSet::new();
    let mut receipt_nonces = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(receipts.len());
    for authority in &receipts {
        let (receipt, receipt_sha256) =
            validate_prior_source_terminal_receipt(authority, expected)?;
        if !source_identities.insert(receipt.source_lmdb_identity)
            || !receipt_nonces.insert(receipt.attempt_nonce.clone())
        {
            bail!("source-terminal receipt set contains a duplicate source identity or attempt");
        }
        validated.push(ValidatedSourceTerminalReceiptV3 {
            authority_sha256: receipt_sha256,
            receipt,
        });
    }
    validated.sort_by(|left, right| left.authority_sha256.cmp(&right.authority_sha256));
    Ok(validated)
}

fn bounded_source_terminal_receipt_authorities(
    authorities: &[NamedFileAuthorityV3],
) -> Result<Vec<&NamedFileAuthorityV3>> {
    let receipts = authorities
        .iter()
        .filter(|authority| {
            authority
                .label
                .starts_with(SOURCE_TERMINAL_CAS_LABEL_PREFIX)
        })
        .take(MAX_FINAL_SOURCE_RECEIPTS + 1)
        .collect::<Vec<_>>();
    if receipts.len() > MAX_FINAL_SOURCE_RECEIPTS {
        bail!(
            "source-terminal receipt set exceeds the hard maximum of {MAX_FINAL_SOURCE_RECEIPTS}"
        );
    }
    Ok(receipts)
}

fn validate_prior_source_terminal_receipt(
    authority: &NamedFileAuthorityV3,
    expected: &PriorSourceReceiptExpectationV3<'_>,
) -> Result<(PoolMigrationSourceTerminalReceiptV3, String)> {
    validate_sha256("prior source-terminal CAS", &authority.sha256)?;
    let expected_service_gid = expected
        .expected_service_gid
        .context("source-terminal certification requires an exact service GID")?;
    let (source_terminal_authority, bytes) = read_validated_source_terminal_certification(
        &FileAuthorityV3 {
            path: authority.path.clone(),
            sha256: authority.sha256.clone(),
        },
        expected.boot_id,
        expected_service_gid,
    )
    .context("validate root-certified source-terminal handoff")?;
    let receipt: PoolMigrationSourceTerminalReceiptV3 =
        serde_json::from_slice(&bytes).context("parse strict prior source-terminal receipt")?;
    if authority.label
        != format!(
            "{SOURCE_TERMINAL_CAS_LABEL_PREFIX}{}",
            receipt.attempt_nonce
        )
    {
        bail!("source-terminal CAS label must end in the exact receipt attempt nonce");
    }
    if receipt.schema != SOURCE_TERMINAL_SCHEMA
        || receipt.status != "verified"
        || receipt.phase != "final-stopped-source"
        || !receipt.source_read_only
        || !receipt.target_audit_deferred
    {
        bail!("prior source-terminal receipt is not a verified deferred-target final source pass");
    }
    validate_receipt_shape(&receipt)?;
    validate_completed_attempt(
        &source_terminal_authority.path,
        &receipt,
        expected.expected_service_gid,
    )?;
    validate_prior_request_and_ack(&receipt)?;
    validate_terminal_cursor(&receipt.terminal_cursor)?;
    validate_source_evidence_metadata(
        &receipt.source_evidence,
        expected.expected_service_gid,
        expected.validate_physical_generation,
    )?;

    if receipt.boot_id != expected.boot_id {
        bail!("prior source-terminal receipt belongs to a different boot");
    }
    if receipt.pool_path != expected.pool_path
        || receipt.pool_lmdb_identity != expected.pool_lmdb_identity
        || receipt.pool_topology_sha256 != expected.pool_topology_sha256
        || receipt.pool_manifest_sha256 != expected.pool_manifest_sha256
        || receipt.pool_topology != *expected.pool_topology
    {
        bail!("prior source-terminal receipt does not bind the exact current Pool authority");
    }
    validate_mask_subset(
        &receipt.stopped_writer_units,
        &receipt.writer_unit_masks,
        expected.stopped_writer_units,
        expected.writer_unit_masks,
    )?;
    if receipt.legacy_worker_template_mask != *expected.legacy_worker_template_mask
        || receipt.legacy_worker_instance_masks != expected.legacy_worker_instance_masks
    {
        bail!(
            "source-terminal receipt does not bind the exact legacy migration-worker activation fence"
        );
    }
    validate_source_read_only_mount_authority(
        &receipt.source_read_only_mounts,
        &receipt.source_path,
        receipt.source_lmdb_identity,
        receipt.source_external_path.as_deref(),
        receipt.source_external_identity,
    )
    .context("revalidate prior source read-only mount authority")?;
    if expected.validate_physical_generation {
        #[cfg(target_os = "linux")]
        bail!(
            "physical source-generation validation requires a retained /proc/self/fd directory authority"
        );
        #[cfg(not(target_os = "linux"))]
        {
            let external = receipt.source_external_path.as_ref().map(|path| {
                hashtree_lmdb::ExternalBlobOptions {
                    base_path: path.clone(),
                    min_bytes: 1,
                    sync: true,
                    pack_target_bytes: None,
                }
            });
            let reader = hashtree_lmdb::LmdbBlobReader::open(&receipt.source_path, external)
                .context("open source-terminal LMDB for generation revalidation")?;
            let generation = reader.environment_generation();
            drop(reader);
            let current = capture_source_generation_fingerprint(
                &receipt.source_path,
                receipt.source_lmdb_identity,
                receipt.source_external_path.as_deref(),
                receipt.source_external_identity,
                generation,
            )?;
            if current != receipt.source_generation {
                bail!("source physical generation differs from its source-terminal receipt");
            }
        }
    }
    Ok((receipt, source_terminal_authority.sha256))
}

fn validate_mask_subset(
    receipt_units: &[String],
    receipt_masks: &[WriterUnitMaskV3],
    final_units: &[String],
    final_masks: &[WriterUnitMaskV3],
) -> Result<()> {
    if receipt_units.is_empty() || receipt_units.len() != receipt_masks.len() {
        bail!("source-terminal receipt has an incomplete source-writer mask set");
    }
    for (unit, mask) in receipt_units.iter().zip(receipt_masks) {
        let index = final_units
            .binary_search(unit)
            .map_err(|_| anyhow::anyhow!("final writer-unit set omits source writer {unit}"))?;
        if final_masks.get(index) != Some(mask) {
            bail!("final writer-mask authority changed for source writer {unit}");
        }
    }
    Ok(())
}

fn validate_receipt_shape(receipt: &PoolMigrationSourceTerminalReceiptV3) -> Result<()> {
    validate_sha256("prior request", &receipt.request_sha256)?;
    validate_sha256("prior acknowledgement", &receipt.acknowledgement_sha256)?;
    validate_sha256("prior worker binary", &receipt.worker_binary.sha256)?;
    validate_sha256("prior worker argv", &receipt.worker_argv_sha256)?;
    validate_sha256("prior systemd fragment", &receipt.systemd_fragment.sha256)?;
    validate_sha256(
        "prior systemd environment file",
        &receipt.systemd_environment_file.sha256,
    )?;
    validate_sha256("prior controller state", &receipt.controller_state_sha256)?;
    validate_sha256("prior source baseline", &receipt.source_baseline_sha256)?;
    validate_sha256("prior source keyset", &receipt.source_keyset_sha256)?;
    validate_sha256(
        "prior source catalog locations",
        &receipt.source_catalog_location_sha256,
    )?;
    validate_sha256("prior source content", &receipt.source_content_sha256)?;
    validate_sha256(
        "prior source evidence manifest",
        &receipt.source_evidence.sha256,
    )?;
    if let Some(external) = &receipt.source_generation.external {
        validate_sha256(
            "prior source external metadata fingerprint",
            &external.metadata_sha256,
        )?;
    }
    validate_sha256("prior Pool topology", &receipt.pool_topology_sha256)?;
    validate_sha256("prior Pool manifest", &receipt.pool_manifest_sha256)?;
    if receipt.source_verified_entries != receipt.source_blob_entries {
        bail!("prior source-terminal verified-entry count differs from its source key count");
    }
    if receipt.source_evidence.entries != receipt.source_verified_entries {
        bail!("prior source evidence entry count differs from its source verification count");
    }
    if receipt.source_legacy_blob_only != (receipt.source_metadata_entries == 0)
        || receipt
            .source_metadata_entries
            .checked_add(receipt.source_blob_only_entries)
            != Some(receipt.source_blob_entries)
        || receipt
            .source_inline_entries
            .checked_add(receipt.source_loose_external_entries)
            .and_then(|entries| entries.checked_add(receipt.source_packed_external_entries))
            != Some(receipt.source_blob_entries)
    {
        bail!("prior source-terminal raw catalog/location summary is internally inconsistent");
    }
    if receipt.main_pid == 0 || receipt.proc_start_time_ticks == 0 {
        bail!("prior source-terminal worker process identity is incomplete");
    }
    Ok(())
}

pub(super) fn validate_source_terminal_receipt_shape(
    receipt: &PoolMigrationSourceTerminalReceiptV3,
) -> Result<()> {
    validate_receipt_shape(receipt)
}

fn validate_completed_attempt(
    receipt_path: &Path,
    receipt: &PoolMigrationSourceTerminalReceiptV3,
    expected_service_gid: Option<u32>,
) -> Result<()> {
    let attempt = receipt_path
        .parent()
        .context("prior source-terminal receipt has no attempt directory")?;
    let namespace = attempt
        .parent()
        .context("prior source-terminal receipt has no attempt namespace")?;
    if receipt_path.file_name().and_then(|name| name.to_str()) != Some(SOURCE_TERMINAL_FILE_NAME)
        || namespace.file_name().and_then(|name| name.to_str()) != Some(ATTEMPT_NAMESPACE_NAME)
        || attempt.file_name().and_then(|name| name.to_str())
            != Some(receipt.attempt_nonce.as_str())
        || receipt.attempt_namespace != namespace
        || receipt.request_path != attempt.join(REQUEST_FILE_NAME)
        || receipt.acknowledgement_path != attempt.join("launch-ack.json")
    {
        bail!("prior source-terminal receipt does not bind its exact completed attempt paths");
    }
    if receipt.source_evidence.path != attempt.join(SOURCE_EVIDENCE_FILE_NAME) {
        bail!("prior source-terminal receipt does not bind its exact source evidence path");
    }
    require_lower_hex("prior attempt nonce", &receipt.attempt_nonce, 64)?;
    let namespace_metadata =
        std::fs::symlink_metadata(namespace).context("inspect prior attempt namespace")?;
    let attempt_metadata =
        std::fs::symlink_metadata(attempt).context("inspect prior attempt directory")?;
    #[cfg(unix)]
    {
        if !namespace_metadata.file_type().is_dir()
            || namespace_metadata.uid() != 0
            || namespace_metadata.mode() & 0o022 != 0
        {
            bail!("prior attempt namespace is not an exact root-owned non-writable directory");
        }
        if !attempt_metadata.file_type().is_dir()
            || attempt_metadata.uid() != 0
            || attempt_metadata.mode() & u32::from(libc::S_ISVTX) == 0
            || attempt_metadata.mode() & 0o030 != 0o030
            || attempt_metadata.mode() & 0o007 != 0
            || expected_service_gid.is_some_and(|gid| attempt_metadata.gid() != gid)
        {
            bail!(
                "prior attempt directory ownership/mode differs from completed-attempt authority"
            );
        }
        let namespace_identity = FileIdentityV3 {
            device: namespace_metadata.dev(),
            inode: namespace_metadata.ino(),
        };
        let attempt_identity = FileIdentityV3 {
            device: attempt_metadata.dev(),
            inode: attempt_metadata.ino(),
        };
        if namespace_identity != receipt.attempt_namespace_identity
            || attempt_identity != receipt.attempt_identity
        {
            bail!("prior completed-attempt directory identity changed");
        }
    }
    Ok(())
}

fn validate_prior_request_and_ack(receipt: &PoolMigrationSourceTerminalReceiptV3) -> Result<()> {
    let request_bytes =
        read_exact_regular_file(&receipt.request_path, 1024 * 1024, "prior launch request")?;
    if sha256_bytes(&request_bytes) != receipt.request_sha256 {
        bail!("prior launch request changed after source-terminal publication");
    }
    let request: PoolMigrationLaunchRequestV3 =
        serde_json::from_slice(&request_bytes).context("parse strict prior launch request")?;
    if request.schema != REQUEST_SCHEMA
        || request.controller.phase != "final-stopped-source"
        || request.nonce != receipt.attempt_nonce
        || request.boot_id != receipt.boot_id
        || request.attempt_namespace != receipt.attempt_namespace
        || request.attempt_namespace_identity != receipt.attempt_namespace_identity
        || request.attempt_identity != receipt.attempt_identity
        || request.binary.path != receipt.worker_binary.path
        || request.binary.sha256 != receipt.worker_binary.sha256
        || request.systemd_unit != receipt.systemd_unit
        || request.systemd_invocation_id != receipt.systemd_invocation_id
        || request.systemd_fragment.path != receipt.systemd_fragment.path
        || request.systemd_fragment.sha256 != receipt.systemd_fragment.sha256
        || request.systemd_environment_file.path != receipt.systemd_environment_file.path
        || request.systemd_environment_file.sha256 != receipt.systemd_environment_file.sha256
        || request.main_pid != receipt.main_pid
        || request.proc_start_time_ticks != receipt.proc_start_time_ticks
        || request.controller.state.sha256 != receipt.controller_state_sha256
        || request.source.lmdb_path != receipt.source_path
        || request.source.lmdb_identity != receipt.source_lmdb_identity
        || request.source.external_path != receipt.source_external_path
        || request.source.external_identity != receipt.source_external_identity
        || request.source.read_only_mounts.as_ref() != Some(&receipt.source_read_only_mounts)
        || request.source.baseline.sha256 != receipt.source_baseline_sha256
        || request.pool.path != receipt.pool_path
        || request.pool.lmdb_identity != receipt.pool_lmdb_identity
        || request.pool.topology.sha256 != receipt.pool_topology_sha256
        || request.cursor.exists
        || request.cursor.path != receipt.terminal_cursor.path
        || sha256_argv(&request.argv) != receipt.worker_argv_sha256
    {
        bail!("prior source-terminal receipt does not exactly bind its launch request");
    }

    let acknowledgement_bytes = read_exact_regular_file(
        &receipt.acknowledgement_path,
        1024 * 1024,
        "prior launch acknowledgement",
    )?;
    if sha256_bytes(&acknowledgement_bytes) != receipt.acknowledgement_sha256 {
        bail!("prior launch acknowledgement changed after source-terminal publication");
    }
    let acknowledgement: Value = serde_json::from_slice(&acknowledgement_bytes)
        .context("parse prior launch acknowledgement")?;
    if acknowledgement.get("schema").and_then(Value::as_str) != Some(ACK_SCHEMA)
        || acknowledgement.get("status").and_then(Value::as_str) != Some("acknowledged")
        || acknowledgement.get("nonce").and_then(Value::as_str)
            != Some(receipt.attempt_nonce.as_str())
        || acknowledgement.get("bootId").and_then(Value::as_str) != Some(receipt.boot_id.as_str())
        || acknowledgement.get("requestSha256").and_then(Value::as_str)
            != Some(receipt.request_sha256.as_str())
        || acknowledgement
            .get("systemdInvocationId")
            .and_then(Value::as_str)
            != Some(receipt.systemd_invocation_id.as_str())
    {
        bail!("prior source-terminal receipt does not exactly bind its launch acknowledgement");
    }
    Ok(())
}

fn validate_terminal_cursor(cursor: &CursorAuthorityV3) -> Result<()> {
    if !cursor.exists || cursor.value.as_deref() != Some("source-complete") {
        bail!("prior source-terminal receipt does not bind a source-complete cursor");
    }
    let expected_sha = cursor
        .sha256
        .as_deref()
        .context("prior source-complete cursor has no SHA-256")?;
    validate_sha256("prior source-complete cursor", expected_sha)?;
    let bytes = read_exact_regular_file(&cursor.path, 1024, "prior source-complete cursor")?;
    if bytes != b"source-complete\n" || sha256_bytes(&bytes) != expected_sha {
        bail!("prior source-terminal receipt/cursor mismatch");
    }
    let parent = cursor
        .path
        .parent()
        .context("prior source-complete cursor has no parent")?;
    let metadata =
        std::fs::symlink_metadata(parent).context("inspect prior source-complete cursor parent")?;
    #[cfg(unix)]
    if (FileIdentityV3 {
        device: metadata.dev(),
        inode: metadata.ino(),
    }) != cursor.parent_identity
    {
        bail!("prior source-complete cursor parent identity changed");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ExpectedPathType {
    Directory,
    Regular,
}

fn snapshot_path(
    path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<FileMutationSnapshotV3> {
    snapshot_path_impl(path, expected_identity, expected_type, label, false)
}

fn snapshot_retained_path(
    path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<FileMutationSnapshotV3> {
    snapshot_path_impl(path, expected_identity, expected_type, label, true)
}

fn snapshot_path_impl(
    path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
    allow_retained_procfd: bool,
) -> Result<FileMutationSnapshotV3> {
    let retained_procfd = allow_retained_procfd && is_proc_self_fd_path(path);
    if !retained_procfd {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize {label} {}", path.display()))?;
        if canonical != path {
            bail!("{label} must be an exact canonical path");
        }
    }
    let metadata = if retained_procfd && is_exact_proc_self_fd_path(path) {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
    .with_context(|| format!("inspect {label}"))?;
    let type_matches = match expected_type {
        ExpectedPathType::Directory => metadata.file_type().is_dir(),
        ExpectedPathType::Regular => metadata.file_type().is_file(),
    };
    if !type_matches {
        bail!("{label} has the wrong file type");
    }
    #[cfg(unix)]
    {
        let snapshot = FileMutationSnapshotV3 {
            identity: FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        };
        if snapshot.identity != expected_identity {
            bail!("{label} identity differs from the current Pool authority");
        }
        Ok(snapshot)
    }
    #[cfg(not(unix))]
    {
        let _ = expected_identity;
        Ok(FileMutationSnapshotV3 {
            identity: FileIdentityV3 {
                device: 0,
                inode: 0,
            },
            len: metadata.len(),
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        })
    }
}

#[cfg(target_os = "linux")]
fn is_proc_self_fd_path(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix("/proc/self/fd") else {
        return false;
    };
    let mut components = relative.components();
    let Some(std::path::Component::Normal(fd)) = components.next() else {
        return false;
    };
    let fd = fd.as_encoded_bytes();
    !fd.is_empty()
        && fd.iter().all(u8::is_ascii_digit)
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[cfg(not(target_os = "linux"))]
fn is_proc_self_fd_path(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn is_exact_proc_self_fd_path(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix("/proc/self/fd") else {
        return false;
    };
    let mut components = relative.components();
    let Some(std::path::Component::Normal(fd)) = components.next() else {
        return false;
    };
    let fd = fd.as_encoded_bytes();
    !fd.is_empty() && fd.iter().all(u8::is_ascii_digit) && components.next().is_none()
}

#[cfg(not(target_os = "linux"))]
fn is_exact_proc_self_fd_path(_path: &Path) -> bool {
    false
}

fn read_exact_regular_file(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    if canonical != path {
        bail!("{label} must be an exact canonical path");
    }
    let path_metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    if !path_metadata.file_type().is_file() {
        bail!("{label} is not a regular file");
    }
    let mut file = File::open(path).with_context(|| format!("open {label}"))?;
    let before = file
        .metadata()
        .with_context(|| format!("inspect open {label}"))?;
    let mut bytes = Vec::with_capacity(before.len().min(maximum) as usize);
    Read::by_ref(&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label}"))?;
    if bytes.len() as u64 > maximum {
        bail!("{label} exceeds its {maximum}-byte authority bound");
    }
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open {label}"))?;
    #[cfg(unix)]
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || after.dev() != path_metadata.dev()
        || after.ino() != path_metadata.ino()
    {
        bail!("{label} changed while it was read");
    }
    Ok(bytes)
}

fn sha256_argv(argv: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-pool-migration-argv/v3\0");
    for argument in argv {
        hasher.update((argument.len() as u64).to_be_bytes());
        hasher.update(argument.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn validate_sha256(label: &str, value: &str) -> Result<()> {
    require_lower_hex(label, value, 64)
}

fn require_lower_hex(label: &str, value: &str, length: usize) -> Result<()> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be exactly {length} lowercase hexadecimal characters");
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn identity(path: &Path) -> FileIdentityV3 {
        let metadata = std::fs::metadata(path).expect("inspect generated path");
        FileIdentityV3 {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[test]
    fn source_receipt_authority_count_is_hard_bounded_before_file_opens() {
        let authorities = (0..=MAX_FINAL_SOURCE_RECEIPTS)
            .map(|index| NamedFileAuthorityV3 {
                label: format!("{SOURCE_TERMINAL_CAS_LABEL_PREFIX}{index:064x}"),
                path: PathBuf::from(format!("/generated/missing/source-terminal-{index}")),
                sha256: "0".repeat(64),
            })
            .collect::<Vec<_>>();
        let accepted =
            bounded_source_terminal_receipt_authorities(&authorities[..MAX_FINAL_SOURCE_RECEIPTS])
                .expect("the exact source receipt cap is accepted");
        assert_eq!(accepted.len(), MAX_FINAL_SOURCE_RECEIPTS);
        let error = bounded_source_terminal_receipt_authorities(&authorities)
            .expect_err("one source receipt above the cap must fail before file validation");
        assert!(
            error.to_string().contains("hard maximum"),
            "unexpected receipt cap error: {error:#}"
        );
    }

    #[test]
    fn retained_procfd_external_fingerprint_is_exact_and_detects_late_write() {
        let temp = tempfile::tempdir().expect("create generated corpus");
        let external = temp.path().join("external");
        std::fs::create_dir(&external).expect("create generated external root");
        let body = external.join("body");
        std::fs::write(&body, b"before").expect("write generated body");

        let expected =
            capture_external_corpus(&external, identity(&external), false).expect("fingerprint");
        let retained = File::open(&external).expect("retain generated external directory");
        let runtime = PathBuf::from(format!("/proc/self/fd/{}", retained.as_raw_fd()));
        let through_procfd = capture_external_corpus(&runtime, identity(&external), true)
            .expect("fingerprint retained procfd corpus");
        assert_eq!(through_procfd, expected);

        std::fs::write(&body, b"after-longer").expect("mutate generated body");
        let after = capture_external_corpus(&runtime, identity(&external), true)
            .expect("fingerprint mutated retained procfd corpus");
        assert_ne!(after, expected);
    }

    #[test]
    fn external_fingerprint_rejects_regular_file_hardlinks() {
        let temp = tempfile::tempdir().expect("create generated corpus");
        let external = temp.path().join("external");
        std::fs::create_dir(&external).expect("create generated external root");
        let body = external.join("body");
        let alias = temp.path().join("alias");
        std::fs::write(&body, b"body").expect("write generated body");
        std::fs::hard_link(&body, &alias).expect("create generated external alias");

        let error = capture_external_corpus(&external, identity(&external), false)
            .expect_err("hardlinked external body must fail closed");
        assert!(format!("{error:#}").contains("must be single-link"));
    }
}
