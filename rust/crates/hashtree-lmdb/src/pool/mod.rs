mod adaptive;
mod catalog;
mod gate;
mod maintenance;
mod maintenance_batch;
mod maintenance_move;
#[cfg(test)]
mod maintenance_tests;
mod member;
mod model;
mod move_catalog;
mod read_only;
mod reader;
mod temperature;
mod temperature_balancer;
mod temperature_catalog;
mod temperature_worker;
#[cfg(test)]
mod tests;

use self::adaptive::AdaptivePoolState;
use self::gate::ConcurrencyGate;
use self::member::{open_member_store, prepare_member_paths, validate_member_config};
use self::model::{LocationRecord, MemberRecord, PoolManifest, MIN_MEMBER_MAP_SIZE_BYTES};
pub use self::model::{
    PoolMaintenanceReport, PoolMemberConfig, PoolMemberId, PoolMemberRuntimePaths, PoolMemberState,
    PoolMemberStatus, PoolStalePending, PoolStalePendingCleanupReport, PoolStoreConfig,
    PoolTemperatureConfig, PoolTemperatureReport,
};
use self::move_catalog::{
    move_cleanup_state_key, move_state_key, rebuild_move_cleanup_member_index_txn,
    validate_move_cleanup_member_index,
};
pub use self::read_only::{
    ReadOnlyPoolCatalogAudit, ReadOnlyPoolManifestMember, ReadOnlyPoolManifestSnapshot,
    ReadOnlyPoolStore,
};
pub use self::reader::{
    PoolCatalogLocation, PoolManifestIdentity, PoolPhysicalAudit, PoolReadBatchItem,
    PoolStoreReader, PoolTerminalAudit,
};
use self::temperature::TemperatureRuntime;
use self::temperature_worker::TemperatureWorker;
use crate::{managed_env::ManagedEnv, pinned_lmdb_data_len, LmdbBlobStore};
use async_trait::async_trait;
use hashtree_core::store::{slice_blob_range, PutManyReport, Store, StoreError, StoreStats};
use hashtree_core::{sha256, to_hex, types::Hash};
use heed::types::{Bytes, Unit};
use heed::{Database, EnvOpenOptions};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CATALOG_DATABASES: u32 = 6;
const CATALOG_MAX_READERS: u32 = 1024;
const MANIFEST_KEY: &[u8] = b"pool-manifest-v1";
const MEMBER_MARKER_NAME: &str = ".hashtree-pool-member-v1";
const EXTERNAL_MARKER_NAME: &str = ".hashtree-pool-external-v1";
const MAX_STALE_PENDING_CLEANUP_ITEMS: usize = 4_096;

#[derive(Default)]
struct RuntimeMembers {
    generation: Option<u64>,
    stores: HashMap<PoolMemberId, Arc<LmdbBlobStore>>,
    read_gates: HashMap<PoolMemberId, Arc<ConcurrencyGate>>,
    write_gates: HashMap<PoolMemberId, Arc<ConcurrencyGate>>,
    errors: HashMap<PoolMemberId, String>,
}

#[derive(Clone)]
pub struct PoolStore {
    inner: Arc<PoolStoreInner>,
}

#[doc(hidden)]
pub struct PoolStoreInner {
    env: ManagedEnv,
    manifest_db: Database<Bytes, Bytes>,
    locations: Database<Bytes, Bytes>,
    by_member: Database<Bytes, Unit>,
    pins: Database<Bytes, Bytes>,
    last_accessed: Database<Bytes, Bytes>,
    temperature_state: Database<Bytes, Bytes>,
    runtime: RwLock<RuntimeMembers>,
    adaptive: Mutex<AdaptivePoolState>,
    temperature_config: PoolTemperatureConfig,
    temperature: Mutex<TemperatureRuntime>,
    temperature_access_counter: AtomicU64,
    temperature_owner: PoolMemberId,
    temperature_cycle: Mutex<()>,
    temperature_worker: TemperatureWorker,
    member_runtime_paths: HashMap<PoolMemberId, PoolMemberRuntimePaths>,
    expected_manifest_sha256: Option<Hash>,
}

impl Deref for PoolStore {
    type Target = PoolStoreInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

fn validate_new_member_paths(
    manifest: &PoolManifest,
    config: &PoolMemberConfig,
) -> Result<(), StoreError> {
    if manifest
        .members
        .iter()
        .any(|member| member.config.path == config.path)
    {
        return Err(StoreError::Other(format!(
            "pool member path is already configured: {}",
            config.path.display()
        )));
    }
    if let Some(external) = config.external_blob_dir.as_ref() {
        if manifest
            .members
            .iter()
            .any(|member| member.config.external_blob_dir.as_ref() == Some(external))
        {
            return Err(StoreError::Other(format!(
                "pool external blob path is already configured: {}",
                external.display()
            )));
        }
    }
    Ok(())
}

fn open_required_catalog_database<K: 'static, D: 'static>(
    env: &ManagedEnv,
    txn: &heed::RoTxn<'_>,
    name: &str,
) -> Result<Database<K, D>, StoreError> {
    env.open_database(txn, Some(name))
        .map_err(map_heed)?
        .ok_or_else(|| StoreError::Other(format!("pool {name} database is missing")))
}

fn validate_controlled_manifest(
    bytes: &[u8],
    expected_sha256: Hash,
    bindings: &HashMap<PoolMemberId, PoolMemberRuntimePaths>,
) -> Result<PoolManifest, StoreError> {
    let actual_sha256 = sha256(bytes);
    if actual_sha256 != expected_sha256 {
        return Err(StoreError::Other(format!(
            "live pool manifest SHA-256 differs from controlled authority: expected {}, found {}",
            to_hex(&expected_sha256),
            to_hex(&actual_sha256)
        )));
    }
    let manifest = decode_manifest(bytes)?;
    validate_controlled_manifest_members(&manifest, bindings)?;
    Ok(manifest)
}

fn validate_controlled_manifest_members(
    manifest: &PoolManifest,
    bindings: &HashMap<PoolMemberId, PoolMemberRuntimePaths>,
) -> Result<(), StoreError> {
    if bindings.len() != manifest.members.len() {
        return Err(StoreError::Other(format!(
            "pinned pool topology has {} members, live manifest has {}",
            bindings.len(),
            manifest.members.len()
        )));
    }
    for member in &manifest.members {
        let binding = bindings.get(&member.id).ok_or_else(|| {
            StoreError::Other(format!(
                "live pool member {} is absent from pinned topology",
                member.id
            ))
        })?;
        if binding.configured_path != member.config.path
            || binding.configured_external_path != member.config.external_blob_dir
        {
            return Err(StoreError::Other(format!(
                "live pool member {} paths differ from pinned topology",
                member.id
            )));
        }
        if !member.config.external_blob_sync {
            return Err(StoreError::Other(format!(
                "controlled migration requires external_blob_sync=true for pool member {}",
                member.id
            )));
        }
    }
    Ok(())
}

impl PoolStore {
    pub fn open<P: AsRef<Path>>(path: P, config: PoolStoreConfig) -> Result<Self, StoreError> {
        config.temperature.validate()?;
        let mut member_runtime_paths = HashMap::new();
        for binding in &config.member_runtime_paths {
            if member_runtime_paths
                .insert(binding.id, binding.clone())
                .is_some()
            {
                return Err(StoreError::Other(format!(
                    "duplicate runtime path binding for pool member {}",
                    binding.id
                )));
            }
            if binding.configured_external_path.is_some() != binding.runtime_external_path.is_some()
            {
                return Err(StoreError::Other(format!(
                    "incomplete external runtime path binding for pool member {}",
                    binding.id
                )));
            }
        }
        let controlled = !member_runtime_paths.is_empty()
            || config.catalog_lmdb_identity.is_some()
            || config.expected_manifest_sha256.is_some();
        if controlled
            && (member_runtime_paths.is_empty()
                || config.catalog_lmdb_identity.is_none()
                || config.expected_manifest_sha256.is_none())
        {
            return Err(StoreError::Other(
                "controlled Pool open requires catalog identity, exact manifest SHA-256, and every member runtime binding".into(),
            ));
        }
        let path = path.as_ref();
        if !controlled {
            fs::create_dir_all(path).map_err(StoreError::Io)?;
        }
        let existing_size = match config.catalog_lmdb_identity {
            Some(identity) => pinned_lmdb_data_len(path, identity)?,
            None => fs::metadata(path.join("data.mdb"))
                .map(|metadata| metadata.len())
                .unwrap_or(0),
        };
        let requested = if controlled {
            config.catalog_map_size_bytes.max(MIN_MEMBER_MAP_SIZE_BYTES)
        } else {
            config
                .catalog_map_size_bytes
                .max(existing_size.saturating_add(existing_size / 10))
                .max(MIN_MEMBER_MAP_SIZE_BYTES)
        };
        let map_size = usize::try_from(requested)
            .map_err(|_| StoreError::Other("pool catalog map size exceeds usize".into()))?;

        let mut options = EnvOpenOptions::new();
        if !controlled {
            options.map_size(map_size);
        }
        options
            .max_dbs(CATALOG_DATABASES)
            .max_readers(CATALOG_MAX_READERS);
        unsafe {
            options.flags(super::env_flags_from_env());
        }
        let env = unsafe {
            match config.catalog_lmdb_identity {
                Some(identity) => ManagedEnv::open_pinned(&options, path, identity),
                None => ManagedEnv::open(&options, path),
            }
        }
        .map_err(|error| {
            StoreError::Other(format!("open pool catalog {}: {error}", path.display()))
        })?;
        if controlled && env.info().map_size < map_size {
            return Err(StoreError::Other(format!(
                "controlled Pool catalog map is {} bytes, below its manifest-owned {} bytes; pre-size it before exact migration",
                env.info().map_size,
                map_size
            )));
        }
        if !controlled {
            let _ = env.clear_stale_readers();
        }
        let (manifest_db, locations, by_member, pins, last_accessed, temperature_state) =
            if controlled {
                let rtxn = env.read_txn().map_err(map_heed)?;
                let manifest_db: Database<Bytes, Bytes> =
                    open_required_catalog_database(&env, &rtxn, "manifest")?;
                let locations: Database<Bytes, Bytes> =
                    open_required_catalog_database(&env, &rtxn, "locations")?;
                let by_member: Database<Bytes, Unit> =
                    open_required_catalog_database(&env, &rtxn, "by_member")?;
                let pins: Database<Bytes, Bytes> =
                    open_required_catalog_database(&env, &rtxn, "pins")?;
                let last_accessed: Database<Bytes, Bytes> =
                    open_required_catalog_database(&env, &rtxn, "last_accessed")?;
                let temperature_state: Database<Bytes, Bytes> =
                    open_required_catalog_database(&env, &rtxn, "temperature_state")?;
                let manifest_bytes = manifest_db
                    .get(&rtxn, MANIFEST_KEY)
                    .map_err(map_heed)?
                    .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
                validate_controlled_manifest(
                    manifest_bytes,
                    config
                        .expected_manifest_sha256
                        .expect("controlled manifest identity checked above"),
                    &member_runtime_paths,
                )?;
                rtxn.commit().map_err(map_heed)?;
                (
                    manifest_db,
                    locations,
                    by_member,
                    pins,
                    last_accessed,
                    temperature_state,
                )
            } else {
                let mut wtxn = env.write_txn().map_err(map_heed)?;
                let manifest_db = env
                    .create_database(&mut wtxn, Some("manifest"))
                    .map_err(map_heed)?;
                let locations = env
                    .create_database(&mut wtxn, Some("locations"))
                    .map_err(map_heed)?;
                let by_member = env
                    .create_database(&mut wtxn, Some("by_member"))
                    .map_err(map_heed)?;
                let pins = env
                    .create_database(&mut wtxn, Some("pins"))
                    .map_err(map_heed)?;
                let last_accessed = env
                    .create_database(&mut wtxn, Some("last_accessed"))
                    .map_err(map_heed)?;
                let temperature_state = env
                    .create_database(&mut wtxn, Some("temperature_state"))
                    .map_err(map_heed)?;
                if manifest_db
                    .get(&wtxn, MANIFEST_KEY)
                    .map_err(map_heed)?
                    .is_none()
                {
                    let bytes = encode_manifest(&PoolManifest::default())?;
                    manifest_db
                        .put(&mut wtxn, MANIFEST_KEY, bytes.as_slice())
                        .map_err(map_heed)?;
                }
                wtxn.commit().map_err(map_heed)?;
                (
                    manifest_db,
                    locations,
                    by_member,
                    pins,
                    last_accessed,
                    temperature_state,
                )
            };
        if !controlled && env.info().map_size < map_size {
            unsafe { env.resize(map_size) }.map_err(map_heed)?;
        }

        if controlled {
            validate_move_cleanup_member_index(&temperature_state, &by_member, &env)?;
        } else {
            let mut wtxn = env.write_txn().map_err(map_heed)?;
            let cleanup_rebuild_started = Instant::now();
            let cleanup_rebuild_count =
                rebuild_move_cleanup_member_index_txn(&temperature_state, &by_member, &mut wtxn)?;
            let cleanup_rebuild_elapsed = cleanup_rebuild_started.elapsed();
            wtxn.commit().map_err(map_heed)?;
            if cleanup_rebuild_count > 0 || cleanup_rebuild_elapsed >= Duration::from_millis(10) {
                eprintln!(
                    "Pool cleanup ownership index rebuild: entries {cleanup_rebuild_count}, elapsed {} us",
                    cleanup_rebuild_elapsed.as_micros()
                );
            }
        }

        let temperature_config = config.temperature.clone();
        let store = Self {
            inner: Arc::new(PoolStoreInner {
                env,
                manifest_db,
                locations,
                by_member,
                pins,
                last_accessed,
                temperature_state,
                runtime: RwLock::new(RuntimeMembers::default()),
                adaptive: Mutex::new(AdaptivePoolState::new(config.member_failure_cooldown)),
                temperature: Mutex::new(TemperatureRuntime::new(
                    temperature_config.candidate_capacity,
                )),
                temperature_config,
                temperature_access_counter: AtomicU64::new(0),
                temperature_owner: PoolMemberId::new(),
                temperature_cycle: Mutex::new(()),
                temperature_worker: TemperatureWorker::default(),
                member_runtime_paths,
                expected_manifest_sha256: config.expected_manifest_sha256,
            }),
        };
        store.refresh_members()?;
        store.start_temperature_worker()?;
        Ok(store)
    }

    fn start_temperature_worker(&self) -> Result<(), StoreError> {
        if !self.temperature_config.enabled {
            return Ok(());
        }
        self.temperature_worker.start(
            Arc::downgrade(&self.inner),
            self.temperature_config.interval,
        )
    }

    /// Stop this process's background temperature balancer.
    ///
    /// Exact maintenance and recovery commands use this before taking
    /// catalog snapshots so the same Pool handle cannot relocate blobs behind
    /// their authority checks.
    pub fn stop_temperature_worker(&self) -> Result<(), StoreError> {
        self.temperature_worker.stop()
    }

    pub fn add_member(&self, config: PoolMemberConfig) -> Result<PoolMemberId, StoreError> {
        self.add_member_inner(config, false)?
            .ok_or_else(|| StoreError::Other("pool member was not added".into()))
    }

    pub(crate) fn ensure_initial_member(
        &self,
        config: PoolMemberConfig,
    ) -> Result<Option<PoolMemberId>, StoreError> {
        if !self.read_manifest()?.members.is_empty() {
            self.refresh_members()?;
            return Ok(None);
        }
        self.add_member_inner(config, true)
    }

    fn add_member_inner(
        &self,
        config: PoolMemberConfig,
        only_if_empty: bool,
    ) -> Result<Option<PoolMemberId>, StoreError> {
        if !self.member_runtime_paths.is_empty() {
            return Err(StoreError::Other(
                "cannot change pool membership while exact runtime paths are pinned".into(),
            ));
        }
        validate_member_config(&config)?;
        let wtxn = self.env.write_txn().map_err(map_heed)?;
        let manifest = self.manifest_from_txn(&wtxn)?;
        if only_if_empty && !manifest.members.is_empty() {
            drop(wtxn);
            self.refresh_members()?;
            return Ok(None);
        }
        validate_new_member_paths(&manifest, &config)?;
        let id = prepare_member_paths(&config, PoolMemberId::new())?;
        drop(wtxn);
        let store = Arc::new(open_member_store(id, &config, None)?);

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut manifest = self.manifest_from_txn(&wtxn)?;
        if only_if_empty && !manifest.members.is_empty() {
            drop(wtxn);
            self.refresh_members()?;
            return Ok(None);
        }
        validate_new_member_paths(&manifest, &config)?;
        if manifest.members.iter().any(|member| member.id == id) {
            return Err(StoreError::Other(format!(
                "pool member identity is already configured: {id}"
            )));
        }
        manifest.members.push(MemberRecord {
            id,
            state: PoolMemberState::Active,
            config,
        });
        manifest.generation = manifest.generation.saturating_add(1);
        self.put_manifest_txn(&mut wtxn, &manifest)?;
        wtxn.commit().map_err(map_heed)?;

        let mut runtime = self
            .runtime
            .write()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        runtime.generation = Some(manifest.generation);
        runtime.stores.insert(id, store);
        let member = manifest
            .members
            .iter()
            .find(|member| member.id == id)
            .expect("new pool member is in committed manifest");
        runtime.read_gates.insert(
            id,
            Arc::new(ConcurrencyGate::new(member.config.max_read_concurrency)),
        );
        runtime.write_gates.insert(
            id,
            Arc::new(ConcurrencyGate::new(member.config.max_write_concurrency)),
        );
        runtime.errors.remove(&id);
        Ok(Some(id))
    }

    pub fn begin_drain(&self, id: PoolMemberId) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut manifest = self.manifest_from_txn(&wtxn)?;
        let has_other_active = manifest
            .members
            .iter()
            .any(|member| member.id != id && member.state == PoolMemberState::Active);
        let member = manifest
            .members
            .iter_mut()
            .find(|member| member.id == id)
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))?;
        if member.state == PoolMemberState::Draining {
            return Ok(());
        }
        if !has_other_active && self.member_has_locations_txn(&wtxn, id)? {
            return Err(StoreError::Other(
                "cannot drain the final member while it still owns blobs".into(),
            ));
        }
        member.state = PoolMemberState::Draining;
        manifest.generation = manifest.generation.saturating_add(1);
        self.put_manifest_txn(&mut wtxn, &manifest)?;
        wtxn.commit().map_err(map_heed)?;
        self.refresh_members()?;
        Ok(())
    }

    pub fn update_member_limits(
        &self,
        id: PoolMemberId,
        capacity_bytes: u64,
        max_read_concurrency: u32,
        max_write_concurrency: u32,
    ) -> Result<(), StoreError> {
        if capacity_bytes == 0 || max_read_concurrency == 0 || max_write_concurrency == 0 {
            return Err(StoreError::Other(
                "pool member capacity and concurrency limits must be non-zero".into(),
            ));
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut manifest = self.manifest_from_txn(&wtxn)?;
        let member = manifest
            .members
            .iter_mut()
            .find(|member| member.id == id)
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))?;
        member.config.capacity_bytes = capacity_bytes;
        member.config.max_read_concurrency = max_read_concurrency;
        member.config.max_write_concurrency = max_write_concurrency;
        manifest.generation = manifest.generation.saturating_add(1);
        self.put_manifest_txn(&mut wtxn, &manifest)?;
        wtxn.commit().map_err(map_heed)?;
        self.refresh_members()
    }

    pub fn update_member_temperature_watermarks(
        &self,
        id: PoolMemberId,
        low_percent: u8,
        high_percent: u8,
    ) -> Result<(), StoreError> {
        if low_percent >= high_percent || high_percent > 100 {
            return Err(StoreError::Other(
                "pool temperature watermarks must satisfy 0 <= low < high <= 100".into(),
            ));
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut manifest = self.manifest_from_txn(&wtxn)?;
        let member = manifest
            .members
            .iter_mut()
            .find(|member| member.id == id)
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))?;
        member.config.temperature_low_watermark_percent = low_percent;
        member.config.temperature_high_watermark_percent = high_percent;
        manifest.generation = manifest.generation.saturating_add(1);
        self.put_manifest_txn(&mut wtxn, &manifest)?;
        wtxn.commit().map_err(map_heed)?;
        self.refresh_members()
    }

    pub fn remove_member(&self, id: PoolMemberId) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut manifest = self.manifest_from_txn(&wtxn)?;
        let index = manifest
            .members
            .iter()
            .position(|member| member.id == id)
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))?;
        if manifest.members[index].state != PoolMemberState::Draining {
            return Err(StoreError::Other(
                "pool member must be draining before removal".into(),
            ));
        }
        if self.member_has_locations_txn(&wtxn, id)? {
            return Err(StoreError::Other(format!(
                "pool member {id} still owns blob(s)"
            )));
        }
        manifest.members.remove(index);
        manifest.generation = manifest.generation.saturating_add(1);
        self.put_manifest_txn(&mut wtxn, &manifest)?;
        wtxn.commit().map_err(map_heed)?;
        self.refresh_members()?;
        Ok(())
    }

    pub fn member(&self, id: PoolMemberId) -> Result<PoolMemberStatus, StoreError> {
        self.refresh_members()?;
        let manifest = self.read_manifest()?;
        let member = manifest
            .members
            .iter()
            .find(|member| member.id == id)
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))?;
        let located_blobs = self.count_member_locations(id)?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        let (logical_bytes, available, last_error) = match runtime.stores.get(&id) {
            Some(store) => match store.stats() {
                Ok(stats) => (stats.total_bytes, true, None),
                Err(error) => (0, false, Some(error.to_string())),
            },
            None => (0, false, runtime.errors.get(&id).cloned()),
        };
        Ok(PoolMemberStatus {
            id,
            state: member.state,
            path: member.config.path.clone(),
            capacity_bytes: member.config.capacity_bytes,
            map_size_bytes: member.config.map_size_bytes,
            external_blob_dir: member.config.external_blob_dir.clone(),
            external_blob_min_bytes: member.config.external_blob_min_bytes,
            external_blob_sync: member.config.external_blob_sync,
            external_pack_target_bytes: member.config.external_pack_target_bytes,
            max_read_concurrency: member.config.max_read_concurrency,
            max_write_concurrency: member.config.max_write_concurrency,
            temperature_low_watermark_percent: member.config.temperature_low_watermark_percent,
            temperature_high_watermark_percent: member.config.temperature_high_watermark_percent,
            logical_bytes,
            located_blobs,
            available,
            last_error,
        })
    }

    pub fn members(&self) -> Result<Vec<PoolMemberStatus>, StoreError> {
        let manifest = self.read_manifest()?;
        manifest
            .members
            .iter()
            .map(|member| self.member(member.id))
            .collect()
    }

    pub fn blob_location(&self, hash: &Hash) -> Result<Option<PoolMemberId>, StoreError> {
        Ok(self
            .read_location(hash)?
            .map(LocationRecord::preferred_member))
    }

    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        if sha256(data) != hash {
            return Err(StoreError::Other(
                "pool rejected bytes that do not match their hash".into(),
            ));
        }

        if let Some(location) = self.read_location(&hash)? {
            match self.read_verified_location(&hash, location) {
                Ok(Some(found)) => {
                    if matches!(location, LocationRecord::Pending { .. }) {
                        self.finalize_pending(hash, location)?;
                    }
                    debug_assert_eq!(sha256(&found), hash);
                    return Ok(false);
                }
                Ok(None) | Err(_) => return self.repair_location(hash, data, location),
            }
        }

        let target = self.choose_write_member(data.len() as u64, None)?;
        let pending = LocationRecord::Pending {
            member: target,
            size: data.len() as u64,
        };
        let location = self.reserve_if_absent(hash, pending)?;
        if location != pending {
            match self.read_verified_location(&hash, location) {
                Ok(Some(found)) => {
                    if matches!(location, LocationRecord::Pending { .. }) {
                        self.finalize_pending(hash, location)?;
                    }
                    debug_assert_eq!(sha256(&found), hash);
                    return Ok(false);
                }
                Ok(None) | Err(_) => return self.repair_location(hash, data, location),
            }
        }

        let target = location.preferred_member();
        let store = self.get_member(target)?;
        match self.write_verified_member(target, &store, hash, data) {
            Ok(inserted) => {
                self.finalize_pending(hash, location)?;
                Ok(inserted)
            }
            Err(_) => {
                let mut excluded = HashSet::new();
                excluded.insert(target);
                self.repair_location_excluding(hash, data, location, excluded)
            }
        }
    }

    pub fn put_many_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        self.put_many_report_sync_with_existing_verification(items, true)
    }

    /// Insert a locally generated content-addressed batch while trusting
    /// catalogued committed locations. Pending locations still take the
    /// ordinary repair path; stored and moving locations need no payload read.
    pub fn put_many_optimistic_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        self.put_many_report_sync_with_existing_verification(items, false)
    }

    fn put_many_report_sync_with_existing_verification(
        &self,
        items: &[(Hash, Vec<u8>)],
        verify_existing: bool,
    ) -> Result<PutManyReport, StoreError> {
        let mut seen = HashSet::new();
        let mut unique = Vec::with_capacity(items.len());
        let mut ordered = Vec::with_capacity(items.len());
        for (hash, data) in items {
            if sha256(data) != *hash {
                return Err(StoreError::Other(
                    "pool rejected batch bytes that do not match their hash".into(),
                ));
            }
            if seen.insert(*hash) {
                unique.push((*hash, data));
                ordered.push((*hash, data.len() as u64));
            }
        }

        let mut inserted = HashSet::new();
        let mut missing = Vec::new();
        for (hash, data) in unique {
            if let Some(location) = self.read_location(&hash)? {
                if (verify_existing || matches!(location, LocationRecord::Pending { .. }))
                    && self.put_sync(hash, data)?
                {
                    inserted.insert(hash);
                }
            } else {
                missing.push((hash, data));
            }
        }
        if missing.is_empty() {
            return Ok(put_many_report(items.len(), &ordered, &inserted));
        }

        let mut reserved_bytes = HashMap::new();
        let mut assignments = Vec::with_capacity(missing.len());
        for (hash, data) in missing {
            let target =
                self.choose_write_member_with_reserved(data.len() as u64, None, &reserved_bytes)?;
            let reserved = reserved_bytes.entry(target).or_insert(0u64);
            *reserved = reserved.saturating_add(data.len() as u64);
            assignments.push((hash, data, target));
        }

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut plans = Vec::with_capacity(assignments.len());
        let mut raced = Vec::new();
        for (hash, data, target) in assignments {
            if self
                .locations
                .get(&wtxn, &hash)
                .map_err(map_heed)?
                .is_some()
            {
                raced.push((hash, data));
                continue;
            }
            let pending = LocationRecord::Pending {
                member: target,
                size: data.len() as u64,
            };
            self.set_location_txn(&mut wtxn, hash, Some(pending))?;
            plans.push((hash, data, target, pending));
        }
        wtxn.commit().map_err(map_heed)?;

        for (hash, data) in raced {
            if self.put_sync(hash, data)? {
                inserted.insert(hash);
            }
        }

        let mut by_target: HashMap<PoolMemberId, Vec<(Hash, &[u8])>> = HashMap::new();
        for (hash, data, target, _) in &plans {
            by_target
                .entry(*target)
                .or_default()
                .push((*hash, data.as_slice()));
        }
        for (target, batch) in by_target {
            let store = self.get_member(target)?;
            let gate = self.member_gate(target, true)?;
            let permit = gate.acquire()?;
            for (hash, _) in &batch {
                if store
                    .get_sync(hash)?
                    .is_some_and(|existing| sha256(&existing) != *hash)
                {
                    store.delete_sync(hash)?;
                }
            }
            let started = Instant::now();
            let result = store.put_many_refs_report_sync(&batch);
            let success = result.is_ok();
            let bytes = batch.iter().map(|(_, data)| data.len()).sum::<usize>();
            self.record_write(target, started.elapsed(), bytes, success);
            let report = match result {
                Ok(report) => report,
                Err(_) => {
                    drop(permit);
                    for (hash, data) in batch {
                        if self.put_sync(hash, data)? {
                            inserted.insert(hash);
                        }
                    }
                    continue;
                }
            };
            inserted.extend(report.inserted_hashes);
            for (hash, _) in &batch {
                self.read_verified_member(target, &store, hash)?
                    .ok_or_else(|| {
                        StoreError::Other(format!(
                            "pool member {target} lost a committed batch write"
                        ))
                    })?;
            }
        }

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        for (hash, _, target, pending) in plans {
            let Some(current) = self.locations.get(&wtxn, &hash).map_err(map_heed)? else {
                continue;
            };
            if LocationRecord::decode(current)? == pending {
                self.set_location_txn(
                    &mut wtxn,
                    hash,
                    Some(LocationRecord::Stored {
                        member: target,
                        size: pending.size(),
                    }),
                )?;
            }
        }
        wtxn.commit().map_err(map_heed)?;
        Ok(put_many_report(items.len(), &ordered, &inserted))
    }

    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_report_sync(items)
            .map(|report| report.inserted)
    }

    pub fn put_many_optimistic_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_optimistic_report_sync(items)
            .map(|report| report.inserted)
    }

    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(location) = self.read_location(hash)? else {
            return Ok(None);
        };
        let data = self.read_verified_location(hash, location)?;
        if data.is_some() && matches!(location, LocationRecord::Pending { .. }) {
            self.finalize_pending(*hash, location)?;
        }
        if data.is_some() {
            self.sample_temperature_access(*hash, location);
        }
        Ok(data)
    }

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(data) = self.get_sync(hash)? else {
            return Ok(None);
        };
        slice_blob_range(&data, start, end_inclusive).map(Some)
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        Ok(self.read_location(hash)?.map(LocationRecord::size))
    }

    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.get_sync(hash)?.is_some())
    }

    pub fn existing_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        sorted_hashes
            .iter()
            .map(|hash| self.locations.get(&rtxn, hash).map(|value| value.is_some()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_heed)
    }

    /// Mark hashes whose pool catalog entries represent committed data.
    ///
    /// Pending records deliberately return false so an interrupted migration
    /// supplies the source bytes again and lets the ordinary repair/finalize
    /// path complete them. Stored and moving records are already durable and
    /// can be skipped by an incremental migration without rereading payloads.
    pub fn committed_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        sorted_hashes
            .iter()
            .map(|hash| {
                self.locations
                    .get(&rtxn, hash)
                    .map_err(map_heed)?
                    .map(LocationRecord::decode)
                    .transpose()
                    .map(|location| {
                        location.is_some_and(|location| {
                            !matches!(location, LocationRecord::Pending { .. })
                        })
                    })
            })
            .collect()
    }

    /// Return the exact catalog state for one sorted source page using a
    /// single read transaction.
    ///
    /// Migration reconciliation uses this to distinguish a terminal
    /// size-matched `Stored` record from `Missing`, crash-left `Pending`, and
    /// non-terminal `Moving` without loading target payload bytes.
    pub fn catalog_locations_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<PoolCatalogLocation>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        sorted_hashes
            .iter()
            .map(|hash| {
                let record = self
                    .locations
                    .get(&rtxn, hash)
                    .map_err(map_heed)?
                    .map(LocationRecord::decode)
                    .transpose()?;
                Ok(PoolCatalogLocation::from_record(record))
            })
            .collect()
    }

    /// Largest map size among members that are available in this process.
    ///
    /// The pool catalog is a separate LMDB environment and is intentionally
    /// not reported as blob capacity.
    pub fn largest_member_map_size_bytes(&self) -> Result<Option<usize>, StoreError> {
        self.refresh_members()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        Ok(runtime
            .stores
            .values()
            .map(|store| store.map_size_bytes())
            .max())
    }

    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        let Some(location) = self.read_location(hash)? else {
            return Ok(false);
        };
        let (members, len) = location.members();
        let mut deleted = false;
        for member in members.into_iter().take(len) {
            if let Ok(store) = self.get_member(member) {
                let gate = self.member_gate(member, true)?;
                let _permit = gate.acquire()?;
                deleted |= store.delete_sync(hash)?;
            }
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        self.set_location_txn(&mut wtxn, *hash, None)?;
        self.pins.delete(&mut wtxn, hash).map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        Ok(deleted)
    }

    /// Delete a batch with one transaction per affected member and one pool
    /// catalog transaction.
    pub fn delete_many_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut seen = HashSet::with_capacity(hashes.len());
        let mut located = Vec::new();
        let mut by_member = HashMap::<PoolMemberId, Vec<Hash>>::new();
        for hash in hashes {
            if !seen.insert(*hash) {
                continue;
            }
            let Some(encoded) = self.locations.get(&rtxn, hash).map_err(map_heed)? else {
                continue;
            };
            let location = LocationRecord::decode(encoded)?;
            let (members, len) = location.members();
            for member in members.into_iter().take(len) {
                by_member.entry(member).or_default().push(*hash);
            }
            located.push(*hash);
        }
        drop(rtxn);

        for (member, member_hashes) in by_member {
            if let Ok(store) = self.get_member(member) {
                let gate = self.member_gate(member, true)?;
                let _permit = gate.acquire()?;
                store.delete_many_sync(&member_hashes)?;
            }
        }

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        for hash in &located {
            self.set_location_txn(&mut wtxn, *hash, None)?;
            self.pins.delete(&mut wtxn, hash).map_err(map_heed)?;
        }
        wtxn.commit().map_err(map_heed)?;
        Ok(located.len())
    }

    /// Clear one exact, bounded set of crash-abandoned `Pending` records.
    ///
    /// This is deliberately narrower than [`Self::delete_many_sync`]: every
    /// live location must still equal the authorized `(hash, member, size)`,
    /// no hash may be pinned or owned by a move, and no member may contain a
    /// physical record for the hash. The catalog and all secondary indexes
    /// change in one transaction and are force-synced before success.
    ///
    /// A caller must stop and fence **every** Pool writer before its strict
    /// read-only audit and keep that fence through this call. This includes
    /// clones and handles in this process: a writer can commit `Pending`, pause
    /// before entering its member gate, and otherwise resume after cleanup.
    /// Catalog and member LMDB environments do not provide one atomic
    /// cross-environment transaction, so this method intentionally makes no
    /// online-safety claim.
    pub fn cleanup_stale_pending_exact_offline_sync(
        &self,
        expected: &[PoolStalePending],
    ) -> Result<PoolStalePendingCleanupReport, StoreError> {
        if expected.is_empty() || expected.len() > MAX_STALE_PENDING_CLEANUP_ITEMS {
            return Err(StoreError::Other(format!(
                "stale Pending cleanup requires 1..={MAX_STALE_PENDING_CLEANUP_ITEMS} exact records"
            )));
        }
        if self.expected_manifest_sha256.is_none() || self.member_runtime_paths.is_empty() {
            return Err(StoreError::Other(
                "stale Pending cleanup requires a controlled Pool open".into(),
            ));
        }
        if self.temperature_config.enabled {
            return Err(StoreError::Other(
                "stale Pending cleanup requires temperature tracking to be disabled".into(),
            ));
        }
        if expected.windows(2).any(|pair| pair[0].hash >= pair[1].hash) {
            return Err(StoreError::Other(
                "stale Pending cleanup records must be strictly hash-sorted and unique".into(),
            ));
        }
        let declared_bytes = expected.iter().try_fold(0u64, |total, item| {
            total.checked_add(item.size).ok_or_else(|| {
                StoreError::Other("stale Pending declared byte total overflow".into())
            })
        })?;

        self.validate_controlled_authority_and_sync()?;

        let mut member_ids = expected.iter().map(|item| item.member).collect::<Vec<_>>();
        member_ids.sort_unstable();
        member_ids.dedup();
        let member_resources = member_ids
            .iter()
            .map(|member| {
                Ok((
                    *member,
                    self.get_member(*member)?,
                    self.member_gate(*member, true)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let _exclusive_permits = member_resources
            .iter()
            .map(|(_, _, gate)| gate.acquire_exclusive())
            .collect::<Result<Vec<_>, StoreError>>()?;

        let ensure_physically_absent = || -> Result<(), StoreError> {
            for (member, store, _) in &member_resources {
                let hashes = expected
                    .iter()
                    .filter(|item| item.member == *member)
                    .map(|item| item.hash)
                    .collect::<Vec<_>>();
                let present = store.existing_hashes_in_sorted_candidates(&hashes)?;
                if let Some(hash) = hashes
                    .iter()
                    .zip(present)
                    .find_map(|(hash, present)| present.then_some(hash))
                {
                    return Err(StoreError::Other(format!(
                        "stale Pending cleanup rejected physically present member record {} on {member}",
                        to_hex(hash)
                    )));
                }
            }
            Ok(())
        };
        ensure_physically_absent()?;

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut exact_pending = 0usize;
        let mut already_missing = 0usize;
        for item in expected {
            let current = self
                .locations
                .get(&wtxn, &item.hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            let authorized = LocationRecord::Pending {
                member: item.member,
                size: item.size,
            };
            match current {
                Some(current) if current == authorized => {
                    exact_pending = exact_pending.saturating_add(1);
                    if self
                        .by_member
                        .get(&wtxn, &member_hash_key(item.member, item.hash))
                        .map_err(map_heed)?
                        .is_none()
                    {
                        return Err(StoreError::Other(format!(
                            "stale Pending cleanup found a missing member index for {}",
                            to_hex(&item.hash)
                        )));
                    }
                }
                None => {
                    already_missing = already_missing.saturating_add(1);
                    if self
                        .by_member
                        .get(&wtxn, &member_hash_key(item.member, item.hash))
                        .map_err(map_heed)?
                        .is_some()
                        || self
                            .last_accessed
                            .get(&wtxn, &item.hash)
                            .map_err(map_heed)?
                            .is_some()
                    {
                        return Err(StoreError::Other(format!(
                            "already-cleared stale Pending record {} retains catalog indexes",
                            to_hex(&item.hash)
                        )));
                    }
                }
                current => {
                    return Err(StoreError::Other(format!(
                        "stale Pending authority no longer matches {}: expected {authorized:?}, found {current:?}",
                        to_hex(&item.hash)
                    )));
                }
            }
            let pin_count = self
                .pins
                .get(&wtxn, &item.hash)
                .map_err(map_heed)?
                .map(decode_pin_count)
                .transpose()?
                .unwrap_or(0);
            if pin_count != 0 {
                return Err(StoreError::Other(format!(
                    "stale Pending cleanup rejected pinned hash {}",
                    to_hex(&item.hash)
                )));
            }
            if self
                .temperature_state
                .get(&wtxn, &move_state_key(item.hash))
                .map_err(map_heed)?
                .is_some()
                || self
                    .temperature_state
                    .get(&wtxn, &move_cleanup_state_key(item.hash))
                    .map_err(map_heed)?
                    .is_some()
            {
                return Err(StoreError::Other(format!(
                    "stale Pending cleanup rejected move-owned hash {}",
                    to_hex(&item.hash)
                )));
            }
        }
        if exact_pending != 0 && already_missing != 0 {
            return Err(StoreError::Other(
                "stale Pending cleanup refuses a partially changed authority set".into(),
            ));
        }

        // Repeat the cross-environment absence proof as late as possible. The
        // operational writer fence remains mandatory for other Pool handles.
        ensure_physically_absent()?;
        if exact_pending != 0 {
            for item in expected {
                self.set_location_txn(&mut wtxn, item.hash, None)?;
                self.pins.delete(&mut wtxn, &item.hash).map_err(map_heed)?;
            }
        }
        wtxn.commit().map_err(map_heed)?;
        self.validate_controlled_authority_and_sync()?;

        let rtxn = self.env.read_txn().map_err(map_heed)?;
        for item in expected {
            if self
                .locations
                .get(&rtxn, &item.hash)
                .map_err(map_heed)?
                .is_some()
                || self
                    .by_member
                    .get(&rtxn, &member_hash_key(item.member, item.hash))
                    .map_err(map_heed)?
                    .is_some()
                || self
                    .pins
                    .get(&rtxn, &item.hash)
                    .map_err(map_heed)?
                    .is_some()
                || self
                    .last_accessed
                    .get(&rtxn, &item.hash)
                    .map_err(map_heed)?
                    .is_some()
            {
                return Err(StoreError::Other(format!(
                    "stale Pending cleanup did not durably clear {}",
                    to_hex(&item.hash)
                )));
            }
        }
        rtxn.commit().map_err(map_heed)?;
        Ok(PoolStalePendingCleanupReport {
            requested: expected.len(),
            declared_bytes,
            already_cleaned: exact_pending == 0,
        })
    }

    pub fn pin_sync(&self, hash: &Hash) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let previous = self
            .pins
            .get(&wtxn, hash)
            .map_err(map_heed)?
            .map(decode_pin_count)
            .transpose()?
            .unwrap_or(0);
        self.pins
            .put(&mut wtxn, hash, &previous.saturating_add(1).to_be_bytes())
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)
    }

    pub fn unpin_sync(&self, hash: &Hash) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let count = self
            .pins
            .get(&wtxn, hash)
            .map_err(map_heed)?
            .map(decode_pin_count)
            .transpose()?
            .unwrap_or(0);
        if count <= 1 {
            self.pins.delete(&mut wtxn, hash).map_err(map_heed)?;
        } else {
            self.pins
                .put(&mut wtxn, hash, &(count - 1).to_be_bytes())
                .map_err(map_heed)?;
        }
        wtxn.commit().map_err(map_heed)
    }

    pub fn pin_count_sync(&self, hash: &Hash) -> Result<u32, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.pins
            .get(&rtxn, hash)
            .map_err(map_heed)?
            .map(decode_pin_count)
            .transpose()
            .map(|count| count.unwrap_or(0))
    }

    pub fn touch_accessed_sync(&self, hash: &Hash, now: u64) -> Result<bool, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        if self.locations.get(&wtxn, hash).map_err(map_heed)?.is_none() {
            return Ok(false);
        }
        let previous = self
            .last_accessed
            .get(&wtxn, hash)
            .map_err(map_heed)?
            .and_then(self::temperature::AccessRecord::decode)
            .unwrap_or_else(|| self::temperature::AccessRecord::new(now));
        let access = self::temperature::AccessRecord {
            last_accessed_at: now,
            ..previous
        }
        .encode();
        self.last_accessed
            .put(&mut wtxn, hash, &access)
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        Ok(true)
    }

    pub fn touch_many_accessed_sync(&self, hashes: &[Hash], now: u64) -> Result<usize, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut updated = 0usize;
        let mut seen = HashSet::new();
        for hash in hashes {
            if !seen.insert(*hash) || self.locations.get(&wtxn, hash).map_err(map_heed)?.is_none() {
                continue;
            }
            let previous = self
                .last_accessed
                .get(&wtxn, hash)
                .map_err(map_heed)?
                .and_then(self::temperature::AccessRecord::decode)
                .unwrap_or_else(|| self::temperature::AccessRecord::new(now));
            let access = self::temperature::AccessRecord {
                last_accessed_at: now,
                ..previous
            }
            .encode();
            self.last_accessed
                .put(&mut wtxn, hash, &access)
                .map_err(map_heed)?;
            updated += 1;
        }
        wtxn.commit().map_err(map_heed)?;
        Ok(updated)
    }

    pub fn last_accessed_at_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.last_accessed
            .get(&rtxn, hash)
            .map_err(map_heed)?
            .map(|bytes| {
                self::temperature::AccessRecord::decode(bytes)
                    .map(|access| access.last_accessed_at)
                    .ok_or_else(|| StoreError::Other("invalid pool access record".into()))
            })
            .transpose()
    }

    pub fn many_last_accessed_at_sync(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<(Hash, u64)>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut values = Vec::new();
        for hash in hashes {
            if let Some(value) = self.last_accessed.get(&rtxn, hash).map_err(map_heed)? {
                let access = self::temperature::AccessRecord::decode(value)
                    .ok_or_else(|| StoreError::Other("invalid pool access record".into()))?;
                values.push((*hash, access.last_accessed_at));
            }
        }
        Ok(values)
    }

    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut hashes = Vec::new();
        for item in self.locations.iter(&rtxn).map_err(map_heed)? {
            let (hash, _) = item.map_err(map_heed)?;
            let hash: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool hash key".into()))?;
            hashes.push(hash);
        }
        Ok(hashes)
    }

    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut stats = StoreStats::default();
        for item in self.locations.iter(&rtxn).map_err(map_heed)? {
            let (_, location) = item.map_err(map_heed)?;
            let location = LocationRecord::decode(location)?;
            stats.count = stats.count.saturating_add(1);
            stats.bytes = stats.bytes.saturating_add(location.size());
        }
        for item in self.pins.iter(&rtxn).map_err(map_heed)? {
            let (hash, count) = item.map_err(map_heed)?;
            if decode_pin_count(count)? == 0 {
                continue;
            }
            let Some(location) = self.locations.get(&rtxn, hash).map_err(map_heed)? else {
                continue;
            };
            stats.pinned_count = stats.pinned_count.saturating_add(1);
            stats.pinned_bytes = stats
                .pinned_bytes
                .saturating_add(LocationRecord::decode(location)?.size());
        }
        Ok(stats)
    }

    /// Return conservative physical usage for quota admission without scanning
    /// the logical Pool catalog.
    ///
    /// Every member LMDB maintains its own totals transactionally, so summing
    /// those counters is constant in the member count and remains accurate
    /// across processes and mixed binary versions. Blobs temporarily duplicated
    /// during a move are counted in both members, which is intentionally
    /// conservative for writable-space admission. Missing member statistics
    /// fail closed rather than undercounting storage.
    pub fn writable_physical_stats(&self) -> Result<StoreStats, StoreError> {
        self.refresh_members()?;
        let manifest = self.read_manifest()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        let mut total = StoreStats::default();
        for member in manifest.members {
            let store = runtime.stores.get(&member.id).ok_or_else(|| {
                let detail = runtime
                    .errors
                    .get(&member.id)
                    .map(String::as_str)
                    .unwrap_or("member store is not open");
                StoreError::Other(format!(
                    "pool member {} unavailable for physical quota accounting: {detail}",
                    member.id
                ))
            })?;
            let stats = store.stats().map_err(|error| {
                StoreError::Other(format!(
                    "pool member {} unavailable for physical quota accounting: {error}",
                    member.id
                ))
            })?;
            let count = u64::try_from(stats.count)
                .map_err(|_| StoreError::Other("pool physical blob count exceeds u64".into()))?;
            let pinned_count = u64::try_from(stats.pinned_count)
                .map_err(|_| StoreError::Other("pool physical pin count exceeds u64".into()))?;
            total.count = total
                .count
                .checked_add(count)
                .ok_or_else(|| StoreError::Other("pool physical blob count overflow".into()))?;
            total.bytes = total
                .bytes
                .checked_add(stats.total_bytes)
                .ok_or_else(|| StoreError::Other("pool physical byte count overflow".into()))?;
            total.pinned_count = total
                .pinned_count
                .checked_add(pinned_count)
                .ok_or_else(|| StoreError::Other("pool physical pin count overflow".into()))?;
            total.pinned_bytes = total
                .pinned_bytes
                .checked_add(stats.pinned_bytes)
                .ok_or_else(|| StoreError::Other("pool physical pinned bytes overflow".into()))?;
        }
        Ok(total)
    }

    pub fn force_sync(&self) -> Result<(), StoreError> {
        self.env.force_sync().map_err(map_heed)?;
        self.refresh_members()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        for store in runtime.stores.values() {
            store.force_sync()?;
        }
        Ok(())
    }

    /// Exhaustively prove every controlled member's physical indexes and
    /// persisted aggregate counters before the first migration mutation.
    ///
    /// Controlled reopen performs only constant-time structural checks so
    /// bounded mapping epochs do not rescan the complete target. The caller
    /// must hold the external writer fence while this one full proof runs and
    /// must keep that fence held until migration completion.
    pub fn validate_controlled_member_state_exact(&self) -> Result<(), StoreError> {
        if self.expected_manifest_sha256.is_none() {
            return Err(StoreError::Other(
                "exact controlled member validation requires an exact manifest SHA-256".into(),
            ));
        }
        self.refresh_members()?;
        let manifest = self.read_manifest()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        if runtime.stores.len() != manifest.members.len() {
            return Err(StoreError::Other(
                "not every authority-pinned Pool member is open for exact validation".into(),
            ));
        }
        for member in manifest.members {
            let store = runtime.stores.get(&member.id).ok_or_else(|| {
                StoreError::Other(format!(
                    "controlled Pool member {} is unavailable for exact validation",
                    member.id
                ))
            })?;
            store.validate_exact_member_state().map_err(|error| {
                StoreError::Other(format!(
                    "controlled Pool member {} failed exact state validation: {error}",
                    member.id
                ))
            })?;
        }
        Ok(())
    }

    /// Revalidate every controlled authority and force the catalog/member
    /// commits durable before an external migration cursor may advance.
    pub fn validate_controlled_authority_and_sync(&self) -> Result<(), StoreError> {
        if self.expected_manifest_sha256.is_none() {
            return Err(StoreError::Other(
                "controlled authority validation requires an exact manifest SHA-256".into(),
            ));
        }
        self.refresh_members()?;
        self.env.force_sync().map_err(map_heed)?;
        {
            let runtime = self
                .runtime
                .read()
                .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
            if runtime.stores.len() != self.member_runtime_paths.len() {
                return Err(StoreError::Other(
                    "not every authority-pinned Pool member is open".into(),
                ));
            }
            for store in runtime.stores.values() {
                store.force_sync()?;
            }
        }
        self.refresh_members()
    }

    fn refresh_members(&self) -> Result<(), StoreError> {
        let (manifest, manifest_sha256) = self.read_manifest_with_identity()?;
        if let Some(expected) = self.expected_manifest_sha256 {
            if manifest_sha256 != expected {
                return Err(StoreError::Other(format!(
                    "live pool manifest SHA-256 differs from controlled authority: expected {}, found {}",
                    to_hex(&expected),
                    to_hex(&manifest_sha256)
                )));
            }
            validate_controlled_manifest_members(&manifest, &self.member_runtime_paths)?;
        }
        let mut runtime = self
            .runtime
            .write()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        if runtime.generation == Some(manifest.generation) {
            return Ok(());
        }

        let configured = manifest
            .members
            .iter()
            .map(|member| member.id)
            .collect::<HashSet<_>>();
        runtime.stores.retain(|id, _| configured.contains(id));
        runtime.read_gates.retain(|id, _| configured.contains(id));
        runtime.write_gates.retain(|id, _| configured.contains(id));
        runtime.errors.retain(|id, _| configured.contains(id));
        for member in &manifest.members {
            let read_gate = runtime
                .read_gates
                .entry(member.id)
                .or_insert_with(|| {
                    Arc::new(ConcurrencyGate::new(member.config.max_read_concurrency))
                })
                .clone();
            read_gate.set_limit(member.config.max_read_concurrency)?;
            let write_gate = runtime
                .write_gates
                .entry(member.id)
                .or_insert_with(|| {
                    Arc::new(ConcurrencyGate::new(member.config.max_write_concurrency))
                })
                .clone();
            write_gate.set_limit(member.config.max_write_concurrency)?;
            if runtime.stores.contains_key(&member.id) {
                continue;
            }
            let config = self.runtime_member_config(member)?;
            let pinned_identity = self
                .member_runtime_paths
                .get(&member.id)
                .map(|binding| binding.lmdb_identity);
            match open_member_store(member.id, &config, pinned_identity) {
                Ok(store) => {
                    runtime.stores.insert(member.id, Arc::new(store));
                    runtime.errors.remove(&member.id);
                }
                Err(error) => {
                    if self.expected_manifest_sha256.is_some() {
                        return Err(StoreError::Other(format!(
                            "controlled Pool member {} failed its exact open: {error}",
                            member.id
                        )));
                    }
                    runtime.errors.insert(member.id, error.to_string());
                }
            }
        }
        runtime.generation = Some(manifest.generation);
        drop(runtime);
        self.adaptive
            .lock()
            .map_err(|_| StoreError::Other("pool adaptive lock poisoned".into()))?
            .retain(&configured);
        Ok(())
    }

    fn runtime_member_config(&self, member: &MemberRecord) -> Result<PoolMemberConfig, StoreError> {
        let Some(binding) = self.member_runtime_paths.get(&member.id) else {
            if self.member_runtime_paths.is_empty() {
                return Ok(member.config.clone());
            }
            return Err(StoreError::Other(format!(
                "pool member {} has no pinned runtime path",
                member.id
            )));
        };
        if binding.configured_path != member.config.path
            || binding.configured_external_path != member.config.external_blob_dir
        {
            return Err(StoreError::Other(format!(
                "pool member {} paths differ from pinned runtime authority",
                member.id
            )));
        }
        let mut config = member.config.clone();
        config.path = binding.runtime_path.clone();
        config.external_blob_dir = binding.runtime_external_path.clone();
        Ok(config)
    }

    fn get_member(&self, id: PoolMemberId) -> Result<Arc<LmdbBlobStore>, StoreError> {
        self.refresh_members()?;
        if let Some(store) = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?
            .stores
            .get(&id)
            .cloned()
        {
            return Ok(store);
        }

        let member = self
            .read_manifest()?
            .members
            .into_iter()
            .find(|member| member.id == id)
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))?;
        let config = self.runtime_member_config(&member)?;
        let pinned_identity = self
            .member_runtime_paths
            .get(&id)
            .map(|binding| binding.lmdb_identity);
        match open_member_store(id, &config, pinned_identity) {
            Ok(store) => {
                let store = Arc::new(store);
                let mut runtime = self
                    .runtime
                    .write()
                    .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
                runtime.stores.insert(id, Arc::clone(&store));
                runtime.errors.remove(&id);
                Ok(store)
            }
            Err(error) => {
                self.record_member_failure(id, false);
                let mut runtime = self
                    .runtime
                    .write()
                    .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
                runtime.errors.insert(id, error.to_string());
                Err(error)
            }
        }
    }

    fn member_state(&self, id: PoolMemberId) -> Result<Option<PoolMemberState>, StoreError> {
        Ok(self
            .read_manifest()?
            .members
            .into_iter()
            .find(|member| member.id == id)
            .map(|member| member.state))
    }

    fn member_gate(
        &self,
        id: PoolMemberId,
        write: bool,
    ) -> Result<Arc<ConcurrencyGate>, StoreError> {
        self.refresh_members()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        let gates = if write {
            &runtime.write_gates
        } else {
            &runtime.read_gates
        };
        gates
            .get(&id)
            .cloned()
            .ok_or_else(|| StoreError::Other(format!("unknown pool member {id}")))
    }

    fn choose_write_member(
        &self,
        incoming_bytes: u64,
        exclude: Option<PoolMemberId>,
    ) -> Result<PoolMemberId, StoreError> {
        self.choose_write_member_with_reserved(incoming_bytes, exclude, &HashMap::new())
    }

    fn choose_write_member_with_reserved(
        &self,
        incoming_bytes: u64,
        exclude: Option<PoolMemberId>,
        reserved_bytes: &HashMap<PoolMemberId, u64>,
    ) -> Result<PoolMemberId, StoreError> {
        let excluded = exclude.into_iter().collect::<HashSet<_>>();
        self.choose_write_member_excluding(incoming_bytes, &excluded, reserved_bytes)
    }

    fn choose_write_member_excluding(
        &self,
        incoming_bytes: u64,
        excluded: &HashSet<PoolMemberId>,
        reserved_bytes: &HashMap<PoolMemberId, u64>,
    ) -> Result<PoolMemberId, StoreError> {
        self.refresh_members()?;
        let manifest = self.read_manifest()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        let mut candidates = Vec::new();
        let mut below_high_watermark = Vec::new();
        for member in manifest.members.iter().filter(|member| {
            member.state == PoolMemberState::Active && !excluded.contains(&member.id)
        }) {
            let Some(store) = runtime.stores.get(&member.id) else {
                continue;
            };
            let stats = match store.stats() {
                Ok(stats) => stats,
                Err(_) => continue,
            };
            let effective_bytes = stats
                .total_bytes
                .saturating_add(reserved_bytes.get(&member.id).copied().unwrap_or(0));
            if member.config.capacity_bytes > 0
                && effective_bytes.saturating_add(incoming_bytes) > member.config.capacity_bytes
            {
                continue;
            }
            candidates.push((member.id, effective_bytes, member.config.capacity_bytes));
            let projected_fill = effective_bytes
                .saturating_add(incoming_bytes)
                .saturating_mul(100)
                .saturating_div(member.config.capacity_bytes)
                .min(100);
            if projected_fill <= u64::from(member.config.temperature_high_watermark_percent) {
                below_high_watermark.push((
                    member.id,
                    effective_bytes,
                    member.config.capacity_bytes,
                ));
            }
        }
        drop(runtime);
        if !below_high_watermark.is_empty() {
            candidates = below_high_watermark;
        }
        self.adaptive
            .lock()
            .map_err(|_| StoreError::Other("pool adaptive lock poisoned".into()))?
            .choose_write(&candidates)
            .ok_or_else(|| StoreError::Other("no writable pool member has capacity".into()))
    }

    fn repair_location(
        &self,
        hash: Hash,
        data: &[u8],
        expected: LocationRecord,
    ) -> Result<bool, StoreError> {
        self.repair_location_excluding(hash, data, expected, HashSet::new())
    }

    fn repair_location_excluding(
        &self,
        hash: Hash,
        data: &[u8],
        expected: LocationRecord,
        mut excluded: HashSet<PoolMemberId>,
    ) -> Result<bool, StoreError> {
        let preferred = expected.preferred_member();
        let mut next = (!excluded.contains(&preferred)
            && self.member_state(preferred)? == Some(PoolMemberState::Active))
        .then_some(preferred);
        let mut last_error = None;
        let (target, inserted) = loop {
            let target = match next.take() {
                Some(target) => target,
                None => match self.choose_write_member_excluding(
                    data.len() as u64,
                    &excluded,
                    &HashMap::new(),
                ) {
                    Ok(target) => target,
                    Err(error) => {
                        return Err(last_error.unwrap_or(error));
                    }
                },
            };
            let result = self
                .get_member(target)
                .and_then(|store| self.write_verified_member(target, &store, hash, data));
            match result {
                Ok(inserted) => break (target, inserted),
                Err(error) => {
                    excluded.insert(target);
                    last_error = Some(error);
                }
            }
        };
        let stored = LocationRecord::Stored {
            member: target,
            size: data.len() as u64,
        };

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let current = self
            .locations
            .get(&wtxn, &hash)
            .map_err(map_heed)?
            .map(LocationRecord::decode)
            .transpose()?;
        if current == Some(expected) {
            self.set_location_txn(&mut wtxn, hash, Some(stored))?;
            wtxn.commit().map_err(map_heed)?;
            return Ok(inserted);
        }
        drop(wtxn);

        if let Some(current) = current {
            if self.read_verified_location(&hash, current)?.is_some() {
                return Ok(false);
            }
        }
        Err(StoreError::Other(format!(
            "pool location changed while repairing {hash:?}"
        )))
    }

    fn read_verified_location(
        &self,
        hash: &Hash,
        location: LocationRecord,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let mut ids = match location {
            LocationRecord::Pending { member, .. } | LocationRecord::Stored { member, .. } => {
                vec![member]
            }
            LocationRecord::Moving { source, target, .. } => vec![target, source],
        };
        self.adaptive
            .lock()
            .map_err(|_| StoreError::Other("pool adaptive lock poisoned".into()))?
            .order_reads(&mut ids);

        let mut first_error = None;
        for id in ids {
            let store = match self.get_member(id) {
                Ok(store) => store,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            match self.read_verified_member(id, &store, hash) {
                Ok(Some(data)) => return Ok(Some(data)),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    fn read_verified_member(
        &self,
        id: PoolMemberId,
        store: &LmdbBlobStore,
        hash: &Hash,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let gate = self.member_gate(id, false)?;
        let _permit = gate.acquire()?;
        let started = Instant::now();
        let result = store.get_sync(hash);
        match result {
            Ok(Some(data)) if sha256(&data) == *hash => {
                self.record_read(id, started.elapsed(), true);
                Ok(Some(data))
            }
            Ok(Some(_)) => {
                self.record_read(id, started.elapsed(), false);
                Err(StoreError::Other(format!(
                    "pool member {id} returned corrupt bytes"
                )))
            }
            Ok(None) => {
                self.record_read(id, started.elapsed(), true);
                Ok(None)
            }
            Err(error) => {
                self.record_read(id, started.elapsed(), false);
                Err(error)
            }
        }
    }

    fn write_verified_member(
        &self,
        id: PoolMemberId,
        store: &LmdbBlobStore,
        hash: Hash,
        data: &[u8],
    ) -> Result<bool, StoreError> {
        let gate = self.member_gate(id, true)?;
        let _permit = gate.acquire()?;
        if let Some(existing) = store.get_sync(&hash)? {
            if sha256(&existing) == hash {
                return Ok(false);
            }
            store.delete_sync(&hash)?;
        }
        let started = Instant::now();
        let result = store.put_sync(hash, data);
        let success = result.is_ok();
        self.record_write(id, started.elapsed(), data.len(), success);
        let inserted = result?;
        let written = store
            .get_sync(&hash)?
            .ok_or_else(|| StoreError::Other(format!("pool member {id} lost a committed write")))?;
        if sha256(&written) != hash {
            self.record_member_failure(id, true);
            return Err(StoreError::Other(format!(
                "pool member {id} committed corrupt bytes"
            )));
        }
        Ok(inserted)
    }

    fn delete_member_blob(
        &self,
        id: PoolMemberId,
        store: &LmdbBlobStore,
        hash: &Hash,
    ) -> Result<bool, StoreError> {
        let gate = self.member_gate(id, true)?;
        let _permit = gate.acquire()?;
        store.delete_sync(hash)
    }

    fn record_read(&self, id: PoolMemberId, elapsed: Duration, success: bool) {
        if let Ok(mut adaptive) = self.adaptive.lock() {
            adaptive.record_read(id, elapsed, success);
        }
    }

    fn record_write(&self, id: PoolMemberId, elapsed: Duration, bytes: usize, success: bool) {
        if let Ok(mut adaptive) = self.adaptive.lock() {
            adaptive.record_write(id, elapsed, bytes, success);
        }
    }

    fn record_member_failure(&self, id: PoolMemberId, write: bool) {
        if write {
            self.record_write(id, Duration::ZERO, 0, false);
        } else {
            self.record_read(id, Duration::ZERO, false);
        }
    }

    fn sample_temperature_access(&self, hash: Hash, location: LocationRecord) {
        if !self.temperature_config.enabled {
            return;
        }
        let sample_rate = u64::from(self.temperature_config.read_sample_rate.max(1));
        let access = self
            .temperature_access_counter
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if !access.is_multiple_of(sample_rate) {
            return;
        }
        if let Ok(mut temperature) = self.temperature.lock() {
            temperature
                .samples
                .observe(hash, location, unix_timestamp_now());
        }
    }
}

#[async_trait]
impl Store for PoolStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_sync(&items)
    }

    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_optimistic_sync(&items)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_range_sync(hash, start, end_inclusive)
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.blob_size_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }

    async fn delete_many(&self, hashes: Vec<Hash>) -> Result<usize, StoreError> {
        self.delete_many_sync(&hashes)
    }

    async fn stats(&self) -> StoreStats {
        PoolStore::stats(self).unwrap_or_default()
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.pin_sync(hash)
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.unpin_sync(hash)
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        self.pin_count_sync(hash).unwrap_or(0)
    }
}

fn encode_manifest(manifest: &PoolManifest) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec_named(manifest)
        .map_err(|error| StoreError::Other(format!("encode pool manifest: {error}")))
}

fn decode_manifest(bytes: &[u8]) -> Result<PoolManifest, StoreError> {
    let manifest: PoolManifest = rmp_serde::from_slice(bytes)
        .map_err(|error| StoreError::Other(format!("decode pool manifest: {error}")))?;
    if manifest.version != 1 {
        return Err(StoreError::Other(format!(
            "unsupported pool manifest version {}",
            manifest.version
        )));
    }
    Ok(manifest)
}

fn member_hash_key(member: PoolMemberId, hash: Hash) -> [u8; 48] {
    let mut key = [0u8; 48];
    key[..16].copy_from_slice(member.as_bytes());
    key[16..].copy_from_slice(&hash);
    key
}

fn decode_pin_count(bytes: &[u8]) -> Result<u32, StoreError> {
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| {
        StoreError::Other("invalid pool pin count".into())
    })?))
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn put_many_report(
    total: usize,
    ordered: &[(Hash, u64)],
    inserted: &HashSet<Hash>,
) -> PutManyReport {
    let mut report = PutManyReport {
        total,
        ..PutManyReport::default()
    };
    for (hash, bytes) in ordered {
        if inserted.contains(hash) {
            report.inserted = report.inserted.saturating_add(1);
            report.inserted_bytes = report.inserted_bytes.saturating_add(*bytes);
            report.inserted_hashes.push(*hash);
        }
    }
    report
}

fn map_heed(error: heed::Error) -> StoreError {
    StoreError::Other(error.to_string())
}
