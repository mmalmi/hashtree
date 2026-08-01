use super::{map_heed, PoolDeleteProtectionChange, PoolDeleteProtectionStatus, PoolStore};
use crate::managed_env::ManagedEnv;
use hashtree_core::store::StoreError;
use hashtree_core::{sha256, to_hex, types::Hash};
use heed::types::Bytes;
use heed::Database;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use {
    std::fs::{File, OpenOptions},
    std::os::fd::AsRawFd,
    std::os::unix::fs::OpenOptionsExt,
};

pub const POOL_DELETE_PROTECTED: &str = "PoolStore logical deletion is durably protected";
const DELETE_PROTECTION_KEY: &[u8] = b"pool-delete-protection-v1";
const DELETE_COORDINATION_FILE: &str = ".hashtree-pool-delete-coordination-v1";
const DELETE_PROTECTION_VERSION: u32 = 1;
const MAX_DELETE_PROTECTION_REASON_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredDeleteProtection {
    version: u32,
    lease_id: Hash,
    reason: String,
    acquired_at_unix_secs: u64,
}

impl PoolStore {
    pub fn delete_protection_status(
        &self,
    ) -> Result<Option<PoolDeleteProtectionStatus>, StoreError> {
        delete_protection_status(&self.env, self.manifest_db)
    }

    pub fn acquire_delete_protection(
        &self,
        lease_id: Hash,
        reason: &str,
    ) -> Result<PoolDeleteProtectionChange, StoreError> {
        require_unix_coordination()?;
        validate_lease_id(lease_id)?;
        validate_reason(reason)?;
        let _coordination = self.acquire_delete_coordination_lock(true)?;
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        if let Some(existing) = self
            .manifest_db
            .get(&wtxn, DELETE_PROTECTION_KEY)
            .map_err(map_heed)?
        {
            let status = decode_status(existing)?;
            if status.lease_id != lease_id || status.reason != reason {
                return Err(StoreError::Other(format!(
                    "PoolStore delete protection is already held by lease {} for {:?}",
                    to_hex(&status.lease_id),
                    status.reason,
                )));
            }
            return Ok(PoolDeleteProtectionChange {
                changed: false,
                status,
            });
        }

        let stored = StoredDeleteProtection {
            version: DELETE_PROTECTION_VERSION,
            lease_id,
            reason: reason.to_owned(),
            acquired_at_unix_secs: unix_timestamp_now()?,
        };
        let encoded = encode_stored(&stored)?;
        self.manifest_db
            .put(&mut wtxn, DELETE_PROTECTION_KEY, &encoded)
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        self.env.force_sync().map_err(map_heed)?;
        Ok(PoolDeleteProtectionChange {
            changed: true,
            status: status_from_stored(stored, sha256(&encoded)),
        })
    }

    /// Hold the exact durable delete-protection record against release while
    /// a long-running online retirement audit uses it as an append-only Pool
    /// authority. Ordinary writes and physical member moves remain available.
    pub fn hold_delete_protection(
        &self,
        lease_id: Hash,
        expected_record_sha256: Hash,
    ) -> Result<PoolDeleteProtectionGuard, StoreError> {
        hold_delete_protection(
            &self.catalog_path,
            &self.env,
            self.manifest_db,
            lease_id,
            expected_record_sha256,
        )
    }

    pub fn release_delete_protection(
        &self,
        lease_id: Hash,
        expected_record_sha256: Hash,
    ) -> Result<PoolDeleteProtectionChange, StoreError> {
        require_unix_coordination()?;
        validate_lease_id(lease_id)?;
        validate_lease_id(expected_record_sha256)?;
        let _coordination = self.acquire_delete_coordination_lock(true)?;
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let existing = self
            .manifest_db
            .get(&wtxn, DELETE_PROTECTION_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("PoolStore delete protection is not active".into()))?;
        let status = decode_status(existing)?;
        if status.lease_id != lease_id {
            return Err(StoreError::Other(format!(
                "PoolStore delete protection lease identity differs: expected {}, found {}",
                to_hex(&lease_id),
                to_hex(&status.lease_id),
            )));
        }
        if status.record_sha256 != expected_record_sha256 {
            return Err(StoreError::Other(format!(
                "PoolStore delete protection record identity differs: expected {}, found {}",
                to_hex(&expected_record_sha256),
                to_hex(&status.record_sha256),
            )));
        }
        self.manifest_db
            .delete(&mut wtxn, DELETE_PROTECTION_KEY)
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        self.env.force_sync().map_err(map_heed)?;
        Ok(PoolDeleteProtectionChange {
            changed: true,
            status,
        })
    }

    pub(super) fn require_deletes_unprotected(&self) -> Result<(), StoreError> {
        if let Some(status) = self.delete_protection_status()? {
            return Err(StoreError::Other(format!(
                "{POOL_DELETE_PROTECTED}: lease {} for {:?}",
                to_hex(&status.lease_id),
                status.reason,
            )));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn acquire_delete_coordination_lock(
        &self,
        exclusive: bool,
    ) -> Result<DeleteCoordinationGuard, StoreError> {
        acquire_delete_coordination_lock(&self.catalog_path, exclusive)
    }

    #[cfg(not(unix))]
    pub(super) fn acquire_delete_coordination_lock(
        &self,
        _exclusive: bool,
    ) -> Result<DeleteCoordinationGuard, StoreError> {
        Ok(DeleteCoordinationGuard)
    }
}

pub(super) fn delete_protection_status(
    env: &ManagedEnv,
    manifest: Database<Bytes, Bytes>,
) -> Result<Option<PoolDeleteProtectionStatus>, StoreError> {
    let rtxn = env.read_txn().map_err(map_heed)?;
    manifest
        .get(&rtxn, DELETE_PROTECTION_KEY)
        .map_err(map_heed)?
        .map(decode_status)
        .transpose()
}

pub(super) fn hold_delete_protection(
    catalog_path: &Path,
    env: &ManagedEnv,
    manifest: Database<Bytes, Bytes>,
    lease_id: Hash,
    expected_record_sha256: Hash,
) -> Result<PoolDeleteProtectionGuard, StoreError> {
    require_unix_coordination()?;
    validate_lease_id(lease_id)?;
    validate_lease_id(expected_record_sha256)?;
    let coordination = acquire_delete_coordination_lock(catalog_path, false)?;
    let status = delete_protection_status(env, manifest)?
        .ok_or_else(|| StoreError::Other("PoolStore delete protection is not active".into()))?;
    if status.lease_id != lease_id {
        return Err(StoreError::Other(format!(
            "PoolStore delete protection lease identity differs: expected {}, found {}",
            to_hex(&lease_id),
            to_hex(&status.lease_id),
        )));
    }
    if status.record_sha256 != expected_record_sha256 {
        return Err(StoreError::Other(format!(
            "PoolStore delete protection record identity differs: expected {}, found {}",
            to_hex(&expected_record_sha256),
            to_hex(&status.record_sha256),
        )));
    }
    Ok(PoolDeleteProtectionGuard {
        _coordination: coordination,
        status,
    })
}

#[cfg(unix)]
fn acquire_delete_coordination_lock(
    catalog_path: &Path,
    exclusive: bool,
) -> Result<DeleteCoordinationGuard, StoreError> {
    let path = catalog_path.join(DELETE_COORDINATION_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| {
            StoreError::Other(format!(
                "open PoolStore delete coordination file {}: {error}",
                path.display()
            ))
        })?;
    let metadata = file.metadata().map_err(StoreError::Io)?;
    if !metadata.is_file() {
        return Err(StoreError::Other(format!(
            "PoolStore delete coordination path is not a regular file: {}",
            path.display()
        )));
    }
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(DeleteCoordinationGuard { file });
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(StoreError::Other(format!(
                "lock PoolStore delete coordination file {}: {error}",
                path.display()
            )));
        }
    }
}

#[cfg(not(unix))]
fn acquire_delete_coordination_lock(
    _catalog_path: &Path,
    _exclusive: bool,
) -> Result<DeleteCoordinationGuard, StoreError> {
    Ok(DeleteCoordinationGuard)
}

fn validate_lease_id(lease_id: Hash) -> Result<(), StoreError> {
    if lease_id == [0; 32] {
        return Err(StoreError::Other(
            "PoolStore delete protection identity must not be all-zero".into(),
        ));
    }
    Ok(())
}

fn validate_reason(reason: &str) -> Result<(), StoreError> {
    if reason.is_empty()
        || reason.trim() != reason
        || reason.len() > MAX_DELETE_PROTECTION_REASON_BYTES
        || !reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" ._:/-".contains(&byte))
    {
        return Err(StoreError::Other(format!(
            "PoolStore delete protection reason must be 1..={MAX_DELETE_PROTECTION_REASON_BYTES} safe ASCII bytes without surrounding whitespace"
        )));
    }
    Ok(())
}

fn unix_timestamp_now() -> Result<u64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| StoreError::Other(format!("system clock precedes Unix epoch: {error}")))
}

fn encode_stored(stored: &StoredDeleteProtection) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec_named(stored)
        .map_err(|error| StoreError::Other(format!("encode PoolStore delete protection: {error}")))
}

fn decode_status(encoded: &[u8]) -> Result<PoolDeleteProtectionStatus, StoreError> {
    let stored: StoredDeleteProtection = rmp_serde::from_slice(encoded).map_err(|error| {
        StoreError::Other(format!("decode PoolStore delete protection: {error}"))
    })?;
    if stored.version != DELETE_PROTECTION_VERSION {
        return Err(StoreError::Other(format!(
            "unsupported PoolStore delete protection version {}",
            stored.version
        )));
    }
    validate_lease_id(stored.lease_id)?;
    validate_reason(&stored.reason)?;
    if stored.acquired_at_unix_secs == 0 {
        return Err(StoreError::Other(
            "PoolStore delete protection acquisition time must be non-zero".into(),
        ));
    }
    Ok(status_from_stored(stored, sha256(encoded)))
}

fn status_from_stored(
    stored: StoredDeleteProtection,
    record_sha256: Hash,
) -> PoolDeleteProtectionStatus {
    PoolDeleteProtectionStatus {
        lease_id: stored.lease_id,
        reason: stored.reason,
        acquired_at_unix_secs: stored.acquired_at_unix_secs,
        record_sha256,
    }
}

#[cfg(unix)]
pub(super) struct DeleteCoordinationGuard {
    file: File,
}

#[cfg(unix)]
impl Drop for DeleteCoordinationGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(not(unix))]
pub(super) struct DeleteCoordinationGuard;

/// An exact durable delete-protection record held against concurrent release.
/// Dropping this value releases only the coordination lock, not the durable
/// protection record.
pub struct PoolDeleteProtectionGuard {
    _coordination: DeleteCoordinationGuard,
    status: PoolDeleteProtectionStatus,
}

impl PoolDeleteProtectionGuard {
    pub fn status(&self) -> &PoolDeleteProtectionStatus {
        &self.status
    }
}

#[cfg(unix)]
fn require_unix_coordination() -> Result<(), StoreError> {
    Ok(())
}

#[cfg(not(unix))]
fn require_unix_coordination() -> Result<(), StoreError> {
    Err(StoreError::Other(
        "durable PoolStore delete protection requires Unix flock coordination".into(),
    ))
}
