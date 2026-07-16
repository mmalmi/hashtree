mod adaptive;
mod gate;
#[cfg(test)]
mod tests;

use self::adaptive::AdaptivePoolState;
use self::gate::ConcurrencyGate;
use crate::{ExternalBlobOptions, LmdbBlobStore};
use async_trait::async_trait;
use hashtree_core::store::{slice_blob_range, Store, StoreError, StoreStats};
use hashtree_core::{sha256, types::Hash};
use heed::types::{Bytes, Unit};
use heed::{Database, EnvOpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

const CATALOG_DATABASES: u32 = 4;
const CATALOG_MAX_READERS: u32 = 1024;
const DEFAULT_CATALOG_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MIN_MEMBER_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024;
const MANIFEST_KEY: &[u8] = b"pool-manifest-v1";
const MEMBER_MARKER_NAME: &str = ".hashtree-pool-member-v1";
const EXTERNAL_MARKER_NAME: &str = ".hashtree-pool-external-v1";

/// Stable identity for one storage member in a local blob pool.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PoolMemberId([u8; 16]);

impl PoolMemberId {
    pub fn new() -> Self {
        Self(*Uuid::new_v4().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Default for PoolMemberId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PoolMemberId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for PoolMemberId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        Uuid::from_bytes(self.0).fmt(formatter)
    }
}

impl FromStr for PoolMemberId {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = Uuid::parse_str(value)
            .map_err(|error| StoreError::Other(format!("invalid pool member id: {error}")))?;
        Ok(Self(*id.as_bytes()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PoolMemberState {
    Active,
    Draining,
}

/// Persistent configuration for one opaque LMDB storage member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMemberConfig {
    pub path: PathBuf,
    pub capacity_bytes: u64,
    pub map_size_bytes: u64,
    pub external_blob_dir: Option<PathBuf>,
    pub external_blob_min_bytes: Option<u64>,
    pub external_blob_sync: bool,
    pub external_pack_target_bytes: Option<u64>,
    pub max_read_concurrency: u32,
    pub max_write_concurrency: u32,
}

impl PoolMemberConfig {
    pub fn new(path: PathBuf, capacity_bytes: u64) -> Self {
        Self {
            path,
            capacity_bytes,
            map_size_bytes: capacity_bytes.max(MIN_MEMBER_MAP_SIZE_BYTES),
            external_blob_dir: None,
            external_blob_min_bytes: None,
            external_blob_sync: true,
            external_pack_target_bytes: None,
            max_read_concurrency: 64,
            max_write_concurrency: 16,
        }
    }

    pub fn with_map_size_bytes(mut self, map_size_bytes: u64) -> Self {
        self.map_size_bytes = map_size_bytes.max(MIN_MEMBER_MAP_SIZE_BYTES);
        self
    }

    pub fn with_external_blobs(
        mut self,
        directory: PathBuf,
        min_bytes: u64,
        sync: bool,
        pack_target_bytes: Option<u64>,
    ) -> Self {
        self.external_blob_dir = Some(directory);
        self.external_blob_min_bytes = Some(min_bytes.max(1));
        self.external_blob_sync = sync;
        self.external_pack_target_bytes = pack_target_bytes.filter(|value| *value > 0);
        self
    }
}

#[derive(Debug, Clone)]
pub struct PoolStoreConfig {
    pub catalog_map_size_bytes: u64,
    pub member_failure_cooldown: Duration,
}

impl Default for PoolStoreConfig {
    fn default() -> Self {
        Self {
            catalog_map_size_bytes: DEFAULT_CATALOG_MAP_SIZE_BYTES,
            member_failure_cooldown: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMemberStatus {
    pub id: PoolMemberId,
    pub state: PoolMemberState,
    pub path: PathBuf,
    pub capacity_bytes: u64,
    pub max_read_concurrency: u32,
    pub max_write_concurrency: u32,
    pub logical_bytes: u64,
    pub located_blobs: u64,
    pub available: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolMaintenanceReport {
    pub examined: usize,
    pub moved: usize,
    pub bytes_moved: u64,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MemberRecord {
    id: PoolMemberId,
    state: PoolMemberState,
    config: PoolMemberConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PoolManifest {
    version: u32,
    generation: u64,
    members: Vec<MemberRecord>,
}

impl Default for PoolManifest {
    fn default() -> Self {
        Self {
            version: 1,
            generation: 0,
            members: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocationRecord {
    Pending {
        member: PoolMemberId,
        size: u64,
    },
    Stored {
        member: PoolMemberId,
        size: u64,
    },
    Moving {
        source: PoolMemberId,
        target: PoolMemberId,
        size: u64,
    },
}

impl LocationRecord {
    fn size(self) -> u64 {
        match self {
            Self::Pending { size, .. } | Self::Stored { size, .. } | Self::Moving { size, .. } => {
                size
            }
        }
    }

    fn preferred_member(self) -> PoolMemberId {
        match self {
            Self::Pending { member, .. } | Self::Stored { member, .. } => member,
            Self::Moving { target, .. } => target,
        }
    }

    fn members(self) -> ([PoolMemberId; 2], usize) {
        match self {
            Self::Pending { member, .. } | Self::Stored { member, .. } => ([member, member], 1),
            Self::Moving { source, target, .. } => ([source, target], 2),
        }
    }

    fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(41);
        match self {
            Self::Pending { member, size } => {
                bytes.push(1);
                bytes.extend_from_slice(member.as_bytes());
                bytes.extend_from_slice(&size.to_be_bytes());
            }
            Self::Stored { member, size } => {
                bytes.push(2);
                bytes.extend_from_slice(member.as_bytes());
                bytes.extend_from_slice(&size.to_be_bytes());
            }
            Self::Moving {
                source,
                target,
                size,
            } => {
                bytes.push(3);
                bytes.extend_from_slice(source.as_bytes());
                bytes.extend_from_slice(target.as_bytes());
                bytes.extend_from_slice(&size.to_be_bytes());
            }
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let member = |slice: &[u8]| -> Result<PoolMemberId, StoreError> {
            let bytes: [u8; 16] = slice
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool member id bytes".into()))?;
            Ok(PoolMemberId(bytes))
        };
        let size = |slice: &[u8]| -> Result<u64, StoreError> {
            Ok(u64::from_be_bytes(slice.try_into().map_err(|_| {
                StoreError::Other("invalid pool location size".into())
            })?))
        };
        match bytes.first().copied() {
            Some(1) if bytes.len() == 25 => Ok(Self::Pending {
                member: member(&bytes[1..17])?,
                size: size(&bytes[17..25])?,
            }),
            Some(2) if bytes.len() == 25 => Ok(Self::Stored {
                member: member(&bytes[1..17])?,
                size: size(&bytes[17..25])?,
            }),
            Some(3) if bytes.len() == 41 => Ok(Self::Moving {
                source: member(&bytes[1..17])?,
                target: member(&bytes[17..33])?,
                size: size(&bytes[33..41])?,
            }),
            _ => Err(StoreError::Other("invalid pool location record".into())),
        }
    }
}

#[derive(Default)]
struct RuntimeMembers {
    generation: Option<u64>,
    stores: HashMap<PoolMemberId, Arc<LmdbBlobStore>>,
    read_gates: HashMap<PoolMemberId, Arc<ConcurrencyGate>>,
    write_gates: HashMap<PoolMemberId, Arc<ConcurrencyGate>>,
    errors: HashMap<PoolMemberId, String>,
}

pub struct PoolStore {
    env: heed::Env,
    manifest_db: Database<Bytes, Bytes>,
    locations: Database<Bytes, Bytes>,
    by_member: Database<Bytes, Unit>,
    pins: Database<Bytes, Bytes>,
    runtime: RwLock<RuntimeMembers>,
    adaptive: Mutex<AdaptivePoolState>,
}

impl PoolStore {
    pub fn open<P: AsRef<Path>>(path: P, config: PoolStoreConfig) -> Result<Self, StoreError> {
        let path = path.as_ref();
        fs::create_dir_all(path).map_err(StoreError::Io)?;
        let existing_size = fs::metadata(path.join("data.mdb"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let requested = config
            .catalog_map_size_bytes
            .max(existing_size.saturating_add(existing_size / 10))
            .max(MIN_MEMBER_MAP_SIZE_BYTES);
        let map_size = usize::try_from(requested)
            .map_err(|_| StoreError::Other("pool catalog map size exceeds usize".into()))?;

        let mut options = EnvOpenOptions::new();
        options
            .map_size(map_size)
            .max_dbs(CATALOG_DATABASES)
            .max_readers(CATALOG_MAX_READERS);
        unsafe {
            options.flags(super::env_flags_from_env());
        }
        let env = unsafe { options.open(path) }.map_err(|error| {
            StoreError::Other(format!("open pool catalog {}: {error}", path.display()))
        })?;
        let _ = env.clear_stale_readers();
        if env.info().map_size < map_size {
            unsafe { env.resize(map_size) }.map_err(map_heed)?;
        }

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

        let store = Self {
            env,
            manifest_db,
            locations,
            by_member,
            pins,
            runtime: RwLock::new(RuntimeMembers::default()),
            adaptive: Mutex::new(AdaptivePoolState::new(config.member_failure_cooldown)),
        };
        store.refresh_members()?;
        Ok(store)
    }

    pub fn add_member(&self, config: PoolMemberConfig) -> Result<PoolMemberId, StoreError> {
        validate_member_config(&config)?;
        let manifest = self.read_manifest()?;
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

        let id = prepare_member_paths(&config, PoolMemberId::new())?;
        let store = Arc::new(open_member_store(id, &config)?);

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut manifest = self.manifest_from_txn(&wtxn)?;
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
        Ok(id)
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
        if !has_other_active && self.count_member_locations_txn(&wtxn, id)? > 0 {
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
        let located = self.count_member_locations_txn(&wtxn, id)?;
        if located != 0 {
            return Err(StoreError::Other(format!(
                "pool member {id} still owns {located} blob(s)"
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
            max_read_concurrency: member.config.max_read_concurrency,
            max_write_concurrency: member.config.max_write_concurrency,
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
        let inserted = self.write_verified_member(target, &store, hash, data)?;
        self.finalize_pending(hash, location)?;
        Ok(inserted)
    }

    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        let mut seen = HashSet::new();
        let mut unique = Vec::with_capacity(items.len());
        for (hash, data) in items {
            if sha256(data) != *hash {
                return Err(StoreError::Other(
                    "pool rejected batch bytes that do not match their hash".into(),
                ));
            }
            if seen.insert(*hash) {
                unique.push((*hash, data));
            }
        }

        let mut inserted = 0usize;
        let mut missing = Vec::new();
        for (hash, data) in unique {
            if self.read_location(&hash)?.is_some() {
                inserted = inserted.saturating_add(usize::from(self.put_sync(hash, data)?));
            } else {
                missing.push((hash, data));
            }
        }
        if missing.is_empty() {
            return Ok(inserted);
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
            inserted = inserted.saturating_add(usize::from(self.put_sync(hash, data)?));
        }

        let mut by_target: HashMap<PoolMemberId, Vec<(Hash, Vec<u8>)>> = HashMap::new();
        for (hash, data, target, _) in &plans {
            by_target
                .entry(*target)
                .or_default()
                .push((*hash, (*data).clone()));
        }
        for (target, batch) in by_target {
            let store = self.get_member(target)?;
            let gate = self.member_gate(target, true)?;
            let _permit = gate.acquire()?;
            for (hash, _) in &batch {
                if store
                    .get_sync(hash)?
                    .is_some_and(|existing| sha256(&existing) != *hash)
                {
                    store.delete_sync(hash)?;
                }
            }
            let started = Instant::now();
            let result = store.put_many_report_sync(&batch);
            let success = result.is_ok();
            let bytes = batch.iter().map(|(_, data)| data.len()).sum::<usize>();
            self.record_write(target, started.elapsed(), bytes, success);
            let report = result?;
            inserted = inserted.saturating_add(report.inserted);
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
        Ok(inserted)
    }

    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(location) = self.read_location(hash)? else {
            return Ok(None);
        };
        let data = self.read_verified_location(hash, location)?;
        if data.is_some() && matches!(location, LocationRecord::Pending { .. }) {
            self.finalize_pending(*hash, location)?;
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

    pub fn maintain(&self, max_items: usize) -> Result<PoolMaintenanceReport, StoreError> {
        let mut report = PoolMaintenanceReport::default();
        if max_items == 0 {
            return Ok(report);
        }
        let draining = self
            .read_manifest()?
            .members
            .into_iter()
            .filter(|member| member.state == PoolMemberState::Draining)
            .map(|member| member.id)
            .collect::<Vec<_>>();

        for source in draining {
            let hashes = self.member_hashes(source, max_items.saturating_sub(report.examined))?;
            for hash in hashes {
                if report.examined >= max_items {
                    return Ok(report);
                }
                report.examined += 1;
                match self.move_from_draining(source, hash) {
                    Ok(Some(bytes)) => {
                        report.moved += 1;
                        report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                    }
                    Ok(None) => {}
                    Err(error) => report.failed.push(format!("{hash:?}: {error}")),
                }
            }
        }
        while report.examined < max_items {
            let Some((source, target)) = self.rebalance_pair()? else {
                break;
            };
            let hashes = self.member_hashes(source, max_items - report.examined)?;
            if hashes.is_empty() {
                break;
            }
            let mut progressed = false;
            for hash in hashes {
                if report.examined >= max_items {
                    break;
                }
                report.examined += 1;
                let Some(location) = self.read_location(&hash)? else {
                    continue;
                };
                if !self.move_improves_balance(source, target, location.size())? {
                    continue;
                }
                match self.move_blob(source, target, hash) {
                    Ok(Some(bytes)) => {
                        report.moved += 1;
                        report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                        progressed = true;
                    }
                    Ok(None) => {}
                    Err(error) => report.failed.push(format!("{hash:?}: {error}")),
                }
            }
            if !progressed {
                break;
            }
        }
        Ok(report)
    }

    fn move_from_draining(
        &self,
        source: PoolMemberId,
        hash: Hash,
    ) -> Result<Option<u64>, StoreError> {
        let Some(location) = self.read_location(&hash)? else {
            return Ok(None);
        };
        let (target, size, moving) = match location {
            LocationRecord::Pending { member, size } | LocationRecord::Stored { member, size }
                if member == source =>
            {
                let target = self.choose_write_member(size, Some(source))?;
                return self.move_blob(source, target, hash);
            }
            LocationRecord::Moving {
                source: moving_source,
                target,
                size,
            } if moving_source == source => (target, size, true),
            _ => return Ok(None),
        };

        self.move_blob_inner(source, target, hash, location, size, moving)
    }

    fn move_blob(
        &self,
        source: PoolMemberId,
        target: PoolMemberId,
        hash: Hash,
    ) -> Result<Option<u64>, StoreError> {
        let Some(location) = self.read_location(&hash)? else {
            return Ok(None);
        };
        let (actual_target, size, moving) = match location {
            LocationRecord::Pending { member, size } | LocationRecord::Stored { member, size }
                if member == source =>
            {
                (target, size, false)
            }
            LocationRecord::Moving {
                source: moving_source,
                target,
                size,
            } if moving_source == source => (target, size, true),
            _ => return Ok(None),
        };
        self.move_blob_inner(source, actual_target, hash, location, size, moving)
    }

    fn move_blob_inner(
        &self,
        source: PoolMemberId,
        target: PoolMemberId,
        hash: Hash,
        location: LocationRecord,
        size: u64,
        moving: bool,
    ) -> Result<Option<u64>, StoreError> {
        let source_store = self.get_member(source)?;
        let target_store = self.get_member(target)?;
        let source_data = match self.read_verified_member(source, &source_store, &hash) {
            Ok(Some(data)) => data,
            Ok(None) | Err(_) if moving => {
                if let Some(target_data) =
                    self.read_verified_member(target, &target_store, &hash)?
                {
                    self.complete_move(hash, source, target, size)?;
                    let _ = self.delete_member_blob(source, &source_store, &hash);
                    return Ok(Some(target_data.len() as u64));
                }
                return Err(StoreError::Other(format!(
                    "draining source {source} does not contain the blob"
                )));
            }
            Ok(None) => {
                return Err(StoreError::Other(format!(
                    "draining source {source} does not contain the blob"
                )))
            }
            Err(error) => return Err(error),
        };

        let moving = LocationRecord::Moving {
            source,
            target,
            size,
        };
        if location != moving {
            self.set_location(hash, Some(moving))?;
        }
        self.write_verified_member(target, &target_store, hash, &source_data)?;
        self.complete_move(hash, source, target, size)?;
        let _ = self.delete_member_blob(source, &source_store, &hash);
        Ok(Some(source_data.len() as u64))
    }

    fn complete_move(
        &self,
        hash: Hash,
        source: PoolMemberId,
        target: PoolMemberId,
        size: u64,
    ) -> Result<(), StoreError> {
        let current = self.read_location(&hash)?;
        match current {
            Some(LocationRecord::Moving {
                source: actual_source,
                target: actual_target,
                ..
            }) if actual_source == source && actual_target == target => self.set_location(
                hash,
                Some(LocationRecord::Stored {
                    member: target,
                    size,
                }),
            ),
            Some(LocationRecord::Stored { member, .. }) if member == target => Ok(()),
            other => Err(StoreError::Other(format!(
                "pool location changed while moving {hash:?}: {other:?}"
            ))),
        }
    }

    fn read_manifest(&self) -> Result<PoolManifest, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.manifest_from_txn(&rtxn)
    }

    fn manifest_from_txn(&self, txn: &heed::RoTxn<'_>) -> Result<PoolManifest, StoreError> {
        let bytes = self
            .manifest_db
            .get(txn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
        decode_manifest(bytes)
    }

    fn put_manifest_txn(
        &self,
        txn: &mut heed::RwTxn<'_>,
        manifest: &PoolManifest,
    ) -> Result<(), StoreError> {
        let bytes = encode_manifest(manifest)?;
        self.manifest_db
            .put(txn, MANIFEST_KEY, bytes.as_slice())
            .map_err(map_heed)
    }

    fn refresh_members(&self) -> Result<(), StoreError> {
        let manifest = self.read_manifest()?;
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
            match open_member_store(member.id, &member.config) {
                Ok(store) => {
                    runtime.stores.insert(member.id, Arc::new(store));
                    runtime.errors.remove(&member.id);
                }
                Err(error) => {
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
        match open_member_store(id, &member.config) {
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
        self.refresh_members()?;
        let manifest = self.read_manifest()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        let mut candidates = Vec::new();
        for member in manifest
            .members
            .iter()
            .filter(|member| member.state == PoolMemberState::Active && Some(member.id) != exclude)
        {
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
        }
        drop(runtime);
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
        let preferred = expected.preferred_member();
        let preferred_store = if self.member_state(preferred)? == Some(PoolMemberState::Active) {
            self.get_member(preferred).ok()
        } else {
            None
        };
        let (target, store) = match preferred_store {
            Some(store) => (preferred, store),
            None => {
                let target = self.choose_write_member(data.len() as u64, Some(preferred))?;
                (target, self.get_member(target)?)
            }
        };
        let inserted = self.write_verified_member(target, &store, hash, data)?;
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

    fn rebalance_pair(&self) -> Result<Option<(PoolMemberId, PoolMemberId)>, StoreError> {
        let manifest = self.read_manifest()?;
        let mut members = Vec::new();
        for member in manifest
            .members
            .into_iter()
            .filter(|member| member.state == PoolMemberState::Active)
        {
            let Ok(store) = self.get_member(member.id) else {
                continue;
            };
            let stats = store.stats()?;
            members.push((member.id, stats.total_bytes, member.config.capacity_bytes));
        }
        if members.len() < 2 {
            return Ok(None);
        }
        let total_bytes = members
            .iter()
            .map(|(_, bytes, _)| *bytes)
            .fold(0u64, u64::saturating_add);
        let total_capacity = members
            .iter()
            .map(|(_, _, capacity)| *capacity)
            .fold(0u64, u64::saturating_add);
        if total_bytes == 0 || total_capacity == 0 {
            return Ok(None);
        }
        let deviation = |bytes: u64, capacity: u64| -> i128 {
            i128::from(bytes) * i128::from(total_capacity)
                - i128::from(total_bytes) * i128::from(capacity)
        };
        let source = members
            .iter()
            .max_by_key(|(_, bytes, capacity)| deviation(*bytes, *capacity))
            .copied();
        let target = members
            .iter()
            .min_by_key(|(_, bytes, capacity)| deviation(*bytes, *capacity))
            .copied();
        match (source, target) {
            (
                Some((source, source_bytes, source_capacity)),
                Some((target, target_bytes, target_capacity)),
            ) if source != target
                && deviation(source_bytes, source_capacity) > 0
                && deviation(target_bytes, target_capacity) < 0 =>
            {
                Ok(Some((source, target)))
            }
            _ => Ok(None),
        }
    }

    fn move_improves_balance(
        &self,
        source: PoolMemberId,
        target: PoolMemberId,
        blob_bytes: u64,
    ) -> Result<bool, StoreError> {
        let members = self.members()?;
        let active = members
            .iter()
            .filter(|member| member.state == PoolMemberState::Active && member.available)
            .collect::<Vec<_>>();
        let Some(source_status) = active.iter().find(|member| member.id == source) else {
            return Ok(false);
        };
        let Some(target_status) = active.iter().find(|member| member.id == target) else {
            return Ok(false);
        };
        if blob_bytes > source_status.logical_bytes
            || target_status.logical_bytes.saturating_add(blob_bytes) > target_status.capacity_bytes
        {
            return Ok(false);
        }
        let total_bytes = active
            .iter()
            .map(|member| member.logical_bytes)
            .fold(0u64, u64::saturating_add);
        let total_capacity = active
            .iter()
            .map(|member| member.capacity_bytes)
            .fold(0u64, u64::saturating_add);
        if total_capacity == 0 {
            return Ok(false);
        }
        let deviation = |bytes: u64, capacity: u64| -> i128 {
            i128::from(bytes) * i128::from(total_capacity)
                - i128::from(total_bytes) * i128::from(capacity)
        };
        let before = deviation(source_status.logical_bytes, source_status.capacity_bytes).abs()
            + deviation(target_status.logical_bytes, target_status.capacity_bytes).abs();
        let after = deviation(
            source_status.logical_bytes - blob_bytes,
            source_status.capacity_bytes,
        )
        .abs()
            + deviation(
                target_status.logical_bytes.saturating_add(blob_bytes),
                target_status.capacity_bytes,
            )
            .abs();
        Ok(after < before)
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

    fn read_location(&self, hash: &Hash) -> Result<Option<LocationRecord>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.locations
            .get(&rtxn, hash)
            .map_err(map_heed)?
            .map(LocationRecord::decode)
            .transpose()
    }

    fn reserve_if_absent(
        &self,
        hash: Hash,
        pending: LocationRecord,
    ) -> Result<LocationRecord, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        if let Some(existing) = self.locations.get(&wtxn, &hash).map_err(map_heed)? {
            return LocationRecord::decode(existing);
        }
        self.set_location_txn(&mut wtxn, hash, Some(pending))?;
        wtxn.commit().map_err(map_heed)?;
        Ok(pending)
    }

    fn finalize_pending(&self, hash: Hash, pending: LocationRecord) -> Result<(), StoreError> {
        if let LocationRecord::Pending { member, size } = pending {
            let mut wtxn = self.env.write_txn().map_err(map_heed)?;
            let current = self
                .locations
                .get(&wtxn, &hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            if current == Some(pending) {
                self.set_location_txn(
                    &mut wtxn,
                    hash,
                    Some(LocationRecord::Stored { member, size }),
                )?;
            }
            wtxn.commit().map_err(map_heed)?;
        }
        Ok(())
    }

    fn set_location(&self, hash: Hash, location: Option<LocationRecord>) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        self.set_location_txn(&mut wtxn, hash, location)?;
        wtxn.commit().map_err(map_heed)
    }

    fn set_location_txn(
        &self,
        txn: &mut heed::RwTxn<'_>,
        hash: Hash,
        location: Option<LocationRecord>,
    ) -> Result<(), StoreError> {
        if let Some(previous) = self.locations.get(txn, &hash).map_err(map_heed)? {
            let previous = LocationRecord::decode(previous)?;
            let (members, len) = previous.members();
            for member in members.into_iter().take(len) {
                self.by_member
                    .delete(txn, &member_hash_key(member, hash))
                    .map_err(map_heed)?;
            }
        }
        match location {
            Some(location) => {
                let encoded = location.encode();
                self.locations.put(txn, &hash, &encoded).map_err(map_heed)?;
                let (members, len) = location.members();
                for member in members.into_iter().take(len) {
                    self.by_member
                        .put(txn, &member_hash_key(member, hash), &())
                        .map_err(map_heed)?;
                }
            }
            None => {
                self.locations.delete(txn, &hash).map_err(map_heed)?;
            }
        }
        Ok(())
    }

    fn count_member_locations(&self, id: PoolMemberId) -> Result<u64, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.count_member_locations_txn(&rtxn, id)
    }

    fn count_member_locations_txn(
        &self,
        txn: &heed::RoTxn<'_>,
        id: PoolMemberId,
    ) -> Result<u64, StoreError> {
        let mut count = 0u64;
        for item in self
            .by_member
            .prefix_iter(txn, id.as_bytes())
            .map_err(map_heed)?
        {
            item.map_err(map_heed)?;
            count = count.saturating_add(1);
        }
        Ok(count)
    }

    fn member_hashes(&self, id: PoolMemberId, limit: usize) -> Result<Vec<Hash>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut hashes = Vec::with_capacity(limit);
        for item in self
            .by_member
            .prefix_iter(&rtxn, id.as_bytes())
            .map_err(map_heed)?
        {
            let (key, _) = item.map_err(map_heed)?;
            if key.len() != 48 {
                return Err(StoreError::Other("invalid pool member index key".into()));
            }
            let hash: Hash = key[16..]
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool member hash key".into()))?;
            hashes.push(hash);
            if hashes.len() >= limit {
                break;
            }
        }
        Ok(hashes)
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

fn validate_member_config(config: &PoolMemberConfig) -> Result<(), StoreError> {
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
    if config.external_blob_dir.is_some() != config.external_blob_min_bytes.is_some() {
        return Err(StoreError::Other(
            "pool external blob directory and threshold must be configured together".into(),
        ));
    }
    Ok(())
}

fn prepare_member_paths(
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

fn verify_member_path(path: &Path, marker_name: &str, id: PoolMemberId) -> Result<(), StoreError> {
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

fn open_member_store(
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

fn map_heed(error: heed::Error) -> StoreError {
    StoreError::Other(error.to_string())
}
