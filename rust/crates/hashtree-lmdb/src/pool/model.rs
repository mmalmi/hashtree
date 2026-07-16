use hashtree_core::store::StoreError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

pub(super) const DEFAULT_CATALOG_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub(super) const MIN_MEMBER_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024;

/// Stable identity for one storage member in a local blob pool.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PoolMemberId(pub(super) [u8; 16]);

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
    pub map_size_bytes: u64,
    pub external_blob_dir: Option<PathBuf>,
    pub external_blob_min_bytes: Option<u64>,
    pub external_blob_sync: bool,
    pub external_pack_target_bytes: Option<u64>,
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
pub(super) struct MemberRecord {
    pub(super) id: PoolMemberId,
    pub(super) state: PoolMemberState,
    pub(super) config: PoolMemberConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PoolManifest {
    pub(super) version: u32,
    pub(super) generation: u64,
    pub(super) members: Vec<MemberRecord>,
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
pub(super) enum LocationRecord {
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
    pub(super) fn size(self) -> u64 {
        match self {
            Self::Pending { size, .. } | Self::Stored { size, .. } | Self::Moving { size, .. } => {
                size
            }
        }
    }

    pub(super) fn preferred_member(self) -> PoolMemberId {
        match self {
            Self::Pending { member, .. } | Self::Stored { member, .. } => member,
            Self::Moving { target, .. } => target,
        }
    }

    pub(super) fn members(self) -> ([PoolMemberId; 2], usize) {
        match self {
            Self::Pending { member, .. } | Self::Stored { member, .. } => ([member, member], 1),
            Self::Moving { source, target, .. } => ([source, target], 2),
        }
    }

    pub(super) fn encode(self) -> Vec<u8> {
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

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
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
