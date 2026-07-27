use super::{EXTERNAL_MARKER_NAME, MEMBER_MARKER_NAME};
use crate::{ExternalBlobOptions, LmdbBlobStore};
use hashtree_core::store::StoreError;
use std::fs;
use std::path::Path;

use super::{PoolMemberConfig, PoolMemberId};

pub(super) fn validate_member_config(config: &PoolMemberConfig) -> Result<(), StoreError> {
    if config.capacity_bytes == 0 {
        return Err(StoreError::Other(
            "pool member capacity must be explicit and non-zero".into(),
        ));
    }
    if config.max_read_concurrency == 0 || config.max_write_concurrency == 0 {
        return Err(StoreError::Other(
            "pool member concurrency limits must be non-zero".into(),
        ));
    }
    if config.temperature_low_watermark_percent >= config.temperature_high_watermark_percent
        || config.temperature_high_watermark_percent > 100
    {
        return Err(StoreError::Other(
            "pool temperature watermarks must satisfy 0 <= low < high <= 100".into(),
        ));
    }
    if config.external_blob_dir.is_some() != config.external_blob_min_bytes.is_some() {
        return Err(StoreError::Other(
            "pool external blob directory and threshold must be configured together".into(),
        ));
    }
    Ok(())
}

pub(super) fn prepare_member_paths(
    config: &PoolMemberConfig,
    proposed: PoolMemberId,
) -> Result<PoolMemberId, StoreError> {
    let id = prepare_identity_path(&config.path, MEMBER_MARKER_NAME, proposed)?;
    if let Some(external) = config.external_blob_dir.as_ref() {
        let external_id = prepare_identity_path(external, EXTERNAL_MARKER_NAME, id)?;
        if external_id != id {
            return Err(StoreError::Other(format!(
                "pool external path belongs to member {external_id}, expected {id}"
            )));
        }
    }
    Ok(id)
}

fn prepare_identity_path(
    path: &Path,
    marker_name: &str,
    proposed: PoolMemberId,
) -> Result<PoolMemberId, StoreError> {
    fs::create_dir_all(path).map_err(StoreError::Io)?;
    let marker = path.join(marker_name);
    if marker.exists() {
        return read_member_marker(&marker);
    }
    let non_marker_entries = fs::read_dir(path)
        .map_err(StoreError::Io)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != marker_name)
        .count();
    if non_marker_entries != 0 {
        return Err(StoreError::Other(format!(
            "refusing to initialize non-empty pool member path without identity marker: {}",
            path.display()
        )));
    }
    fs::write(&marker, format!("{proposed}\n")).map_err(StoreError::Io)?;
    Ok(proposed)
}

fn read_member_marker(path: &Path) -> Result<PoolMemberId, StoreError> {
    let value = fs::read_to_string(path).map_err(StoreError::Io)?;
    value.trim().parse()
}

pub(super) fn verify_member_path(
    path: &Path,
    marker_name: &str,
    id: PoolMemberId,
) -> Result<(), StoreError> {
    let marker = path.join(marker_name);
    let actual = read_member_marker(&marker).map_err(|error| {
        StoreError::Other(format!(
            "pool member identity unavailable at {}: {error}",
            path.display()
        ))
    })?;
    if actual != id {
        return Err(StoreError::Other(format!(
            "pool member identity mismatch at {}: found {actual}, expected {id}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn open_member_store(
    id: PoolMemberId,
    config: &PoolMemberConfig,
) -> Result<LmdbBlobStore, StoreError> {
    verify_member_path(&config.path, MEMBER_MARKER_NAME, id)?;
    let external = match (
        config.external_blob_dir.as_ref(),
        config.external_blob_min_bytes,
    ) {
        (Some(path), Some(min_bytes)) => {
            verify_member_path(path, EXTERNAL_MARKER_NAME, id)?;
            Some(ExternalBlobOptions {
                base_path: path.clone(),
                min_bytes: usize::try_from(min_bytes).map_err(|_| {
                    StoreError::Other("pool external blob threshold exceeds usize".into())
                })?,
                sync: config.external_blob_sync,
                pack_target_bytes: config
                    .external_pack_target_bytes
                    .map(usize::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Other("pool external pack target exceeds usize".into())
                    })?,
            })
        }
        (None, None) => None,
        _ => {
            return Err(StoreError::Other(
                "invalid pool external blob configuration".into(),
            ))
        }
    };
    let map_size = usize::try_from(config.map_size_bytes)
        .map_err(|_| StoreError::Other("pool member map size exceeds usize".into()))?;
    LmdbBlobStore::with_exact_map_size_and_external_blob_options(&config.path, map_size, external)
        .map_err(|error| {
            StoreError::Other(format!(
                "open pool member {id} at {}: {error}",
                config.path.display()
            ))
        })
}
