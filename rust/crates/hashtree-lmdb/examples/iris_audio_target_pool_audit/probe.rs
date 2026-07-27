use crate::config::ValidatedConfig;
use hashtree_core::{sha256, to_hex, Hash};
use hashtree_lmdb::{
    ExternalBlobOptions, LmdbBlobReader, PoolCatalogLocation, PoolManifestIdentity, PoolMemberId,
    PoolStoreConfig, PoolStoreReader,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
enum SourceStatus {
    NotProbed,
    Missing,
    Valid,
    Corrupt,
    Error(String),
}

impl SourceStatus {
    fn label(&self) -> String {
        match self {
            Self::NotProbed => "not-probed".into(),
            Self::Missing => "missing".into(),
            Self::Valid => "hash-valid".into(),
            Self::Corrupt => "hash-mismatch".into(),
            Self::Error(error) => format!("error: {error}"),
        }
    }

    fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    fn is_corrupt(&self) -> bool {
        matches!(self, Self::Corrupt)
    }
}

struct SourceBatch {
    states: BTreeMap<Hash, SourceStatus>,
    valid_data: BTreeMap<Hash, Vec<u8>>,
}

pub struct FallbackReader {
    name: String,
    reader: LmdbBlobReader,
}

pub struct ProbeContext {
    pool: PoolStoreReader,
    pool_manifest_identity: PoolManifestIdentity,
    expected_pool_members: Vec<PoolMemberId>,
    target_members: Vec<PoolMemberId>,
    fallback_tiers: Vec<FallbackReader>,
    read_limit_bytes: u64,
}

#[derive(Debug)]
pub struct HashProbe {
    pub catalog_state: String,
    pub catalog_candidates: Vec<String>,
    pub catalog_target_membership: bool,
    pub catalog_error: Option<String>,
    pub target_members: BTreeMap<String, String>,
    pub fallback_tiers: BTreeMap<String, String>,
    pub target_witness: Option<String>,
    pub fallback_witness: Option<String>,
    pub residency: &'static str,
    pub data: Option<Vec<u8>>,
}

impl ProbeContext {
    pub fn open(config: &ValidatedConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let mut pool_config = PoolStoreConfig::default();
        pool_config.temperature.enabled = false;
        let pool = PoolStoreReader::open_with_unavailable_members_for_audit(
            &config.config.pool_catalog,
            pool_config,
        )?;
        let pool_manifest_identity = pool.manifest_identity();
        let actual_members = pool_manifest_identity.member_ids.clone();
        if actual_members != config.expected_pool_members {
            return Err(format!(
                "Pool manifest member set does not match pinned expectedPoolMembers: expected [{}], got [{}]",
                member_labels(&config.expected_pool_members).join(","),
                member_labels(&actual_members).join(",")
            )
            .into());
        }

        let mut fallback_tiers = Vec::with_capacity(config.config.fallback_tiers.len());
        for tier in &config.config.fallback_tiers {
            let external = tier
                .external_blob_dir
                .as_ref()
                .map(|base_path| ExternalBlobOptions {
                    base_path: PathBuf::from(base_path),
                    min_bytes: 1,
                    sync: false,
                    pack_target_bytes: None,
                });
            fallback_tiers.push(FallbackReader {
                name: tier.name.clone(),
                reader: LmdbBlobReader::open(&tier.lmdb_path, external)?,
            });
        }
        Ok(Self {
            pool,
            pool_manifest_identity,
            expected_pool_members: config.expected_pool_members.clone(),
            target_members: config.target_members.clone(),
            fallback_tiers,
            read_limit_bytes: config.config.read_limit_bytes,
        })
    }

    pub fn pool_manifest_sha256(&self) -> String {
        to_hex(&self.pool_manifest_identity.sha256)
    }

    pub fn pool_manifest_generation(&self) -> u64 {
        self.pool_manifest_identity.generation
    }

    pub fn pool_manifest_identity(&self) -> PoolManifestIdentity {
        self.pool_manifest_identity.clone()
    }

    pub fn expected_pool_member_labels(&self) -> Vec<String> {
        member_labels(&self.expected_pool_members)
    }

    pub fn target_member_labels(&self) -> Vec<String> {
        member_labels(&self.target_members)
    }

    pub fn fallback_tier_names(&self) -> Vec<String> {
        self.fallback_tiers
            .iter()
            .map(|tier| tier.name.clone())
            .collect()
    }

    pub fn probe_hashes(
        &self,
        requested_hashes: &[Hash],
    ) -> Result<BTreeMap<Hash, HashProbe>, Box<dyn std::error::Error>> {
        let hashes = requested_hashes
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if hashes.is_empty() {
            return Ok(BTreeMap::new());
        }

        let (catalog, catalog_error) = match self.pool.blob_catalog_locations(&hashes) {
            Ok(locations) => (locations.into_iter().map(Some).collect::<Vec<_>>(), None),
            Err(error) => (
                vec![None; hashes.len()],
                Some(format!("Pool catalog read failed: {error}")),
            ),
        };

        let mut target_batches = BTreeMap::new();
        for member in &self.target_members {
            let batch = probe_source(
                &hashes,
                self.read_limit_bytes,
                |values| {
                    self.pool
                        .member_existing_hashes_in_sorted_candidates(*member, values)
                },
                |values, limit| {
                    self.pool
                        .read_member_hashes_bounded_unverified(*member, values, limit)
                },
            );
            target_batches.insert(*member, batch);
        }

        let mut need_fallback = Vec::new();
        for (index, hash) in hashes.iter().enumerate() {
            let terminal_target = catalog[index]
                .as_ref()
                .and_then(|location| terminal_target_member(location, &self.target_members));
            let has_catalog_witness = terminal_target.is_some_and(|member| {
                target_batches
                    .get(&member)
                    .and_then(|batch| batch.states.get(hash))
                    .is_some_and(SourceStatus::is_valid)
            });
            let has_any_target = self.target_members.iter().any(|member| {
                target_batches
                    .get(member)
                    .and_then(|batch| batch.states.get(hash))
                    .is_some_and(SourceStatus::is_valid)
            });
            if !has_catalog_witness && !has_any_target {
                need_fallback.push(*hash);
            }
        }

        let mut fallback_batches = BTreeMap::new();
        for tier in &self.fallback_tiers {
            let mut batch = empty_source_batch(&hashes);
            if !need_fallback.is_empty() {
                let probed = probe_source(
                    &need_fallback,
                    self.read_limit_bytes,
                    |values| tier.reader.existing_hashes_in_sorted_candidates(values),
                    |values, limit| tier.reader.read_hashes_bounded(values, limit),
                );
                for hash in &need_fallback {
                    if let Some(state) = probed.states.get(hash) {
                        batch.states.insert(*hash, state.clone());
                    }
                    if let Some(data) = probed.valid_data.get(hash) {
                        batch.valid_data.insert(*hash, data.clone());
                    }
                }
            }
            fallback_batches.insert(tier.name.clone(), batch);
        }

        let mut probes = BTreeMap::new();
        for (index, hash) in hashes.into_iter().enumerate() {
            let location = catalog[index];
            let candidates = location
                .as_ref()
                .map(catalog_candidates)
                .unwrap_or_default();
            let terminal_target = location
                .as_ref()
                .and_then(|location| terminal_target_member(location, &self.target_members));
            let mut target_states = BTreeMap::new();
            let mut fallback_states = BTreeMap::new();
            let mut target_witness = None;
            let mut fallback_witness = None;
            let mut data = None;

            for member in &self.target_members {
                let batch = target_batches
                    .get(member)
                    .ok_or_else(|| format!("target probe omitted member {member}"))?;
                let state = batch
                    .states
                    .get(&hash)
                    .ok_or_else(|| format!("target member {member} omitted a requested hash"))?;
                let label = member.to_string();
                target_states.insert(label.clone(), state.label());
                if state.is_valid() && terminal_target == Some(*member) && target_witness.is_none()
                {
                    target_witness = Some(label.clone());
                    data = batch.valid_data.get(&hash).cloned();
                }
            }
            if target_witness.is_none() {
                for member in &self.target_members {
                    let batch = target_batches
                        .get(member)
                        .ok_or_else(|| format!("target probe omitted member {member}"))?;
                    if batch.states.get(&hash).is_some_and(SourceStatus::is_valid) {
                        target_witness = Some(member.to_string());
                        data = batch.valid_data.get(&hash).cloned();
                        break;
                    }
                }
            }
            for tier in &self.fallback_tiers {
                let batch = fallback_batches
                    .get(&tier.name)
                    .ok_or_else(|| format!("fallback probe omitted tier {}", tier.name))?;
                let state = batch.states.get(&hash).ok_or_else(|| {
                    format!("fallback tier {} omitted a requested hash", tier.name)
                })?;
                fallback_states.insert(tier.name.clone(), state.label());
                if state.is_valid() && fallback_witness.is_none() {
                    fallback_witness = Some(tier.name.clone());
                    if data.is_none() {
                        data = batch.valid_data.get(&hash).cloned();
                    }
                }
            }

            let catalog_witness = terminal_target.is_some_and(|member| {
                target_batches
                    .get(&member)
                    .and_then(|batch| batch.states.get(&hash))
                    .is_some_and(SourceStatus::is_valid)
            });
            let target_valid_anywhere = self.target_members.iter().any(|member| {
                target_batches
                    .get(member)
                    .and_then(|batch| batch.states.get(&hash))
                    .is_some_and(SourceStatus::is_valid)
            });
            let target_error = self.target_members.iter().any(|member| {
                target_batches
                    .get(member)
                    .and_then(|batch| batch.states.get(&hash))
                    .is_some_and(SourceStatus::is_error)
            });
            let fallback_error = self.fallback_tiers.iter().any(|tier| {
                fallback_batches
                    .get(&tier.name)
                    .and_then(|batch| batch.states.get(&hash))
                    .is_some_and(SourceStatus::is_error)
            });
            let any_corrupt = self.target_members.iter().any(|member| {
                target_batches
                    .get(member)
                    .and_then(|batch| batch.states.get(&hash))
                    .is_some_and(SourceStatus::is_corrupt)
            }) || self.fallback_tiers.iter().any(|tier| {
                fallback_batches
                    .get(&tier.name)
                    .and_then(|batch| batch.states.get(&hash))
                    .is_some_and(SourceStatus::is_corrupt)
            });

            let residency = if catalog_witness && catalog_error.is_none() {
                "target-valid"
            } else if catalog_error.is_some() || target_error {
                "unknown"
            } else if target_valid_anywhere {
                "catalog-mismatch"
            } else if fallback_witness.is_some() {
                "fallback-only"
            } else if fallback_error {
                "unknown"
            } else if any_corrupt {
                "corrupt"
            } else {
                "missing"
            };
            probes.insert(
                hash,
                HashProbe {
                    catalog_state: location
                        .as_ref()
                        .map(catalog_state_label)
                        .unwrap_or("error")
                        .into(),
                    catalog_candidates: candidates.iter().map(ToString::to_string).collect(),
                    catalog_target_membership: terminal_target.is_some(),
                    catalog_error: catalog_error.clone(),
                    target_members: target_states,
                    fallback_tiers: fallback_states,
                    target_witness,
                    fallback_witness,
                    residency,
                    data,
                },
            );
        }
        Ok(probes)
    }
}

pub fn verify_pool_manifest_unchanged(
    config: &ValidatedConfig,
    initial: &PoolManifestIdentity,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let reopened = PoolStoreReader::open_with_unavailable_members_for_audit(
        &config.config.pool_catalog,
        pool_config,
    )?;
    let terminal = reopened.manifest_identity();
    if terminal != *initial {
        return Err(format!(
            "Pool manifest changed during audit: initial generation {} SHA256 {}, terminal generation {} SHA256 {}",
            initial.generation,
            to_hex(&initial.sha256),
            terminal.generation,
            to_hex(&terminal.sha256),
        )
        .into());
    }
    if terminal.member_ids != config.expected_pool_members {
        return Err("terminal Pool manifest member IDs differ from expectedPoolMembers".into());
    }
    Ok(())
}

fn member_labels(members: &[PoolMemberId]) -> Vec<String> {
    members.iter().map(ToString::to_string).collect()
}

fn terminal_stored_member(location: &PoolCatalogLocation) -> Option<PoolMemberId> {
    match location {
        PoolCatalogLocation::Stored { member, .. } => Some(*member),
        PoolCatalogLocation::Missing
        | PoolCatalogLocation::Pending { .. }
        | PoolCatalogLocation::Moving { .. } => None,
    }
}

fn terminal_target_member(
    location: &PoolCatalogLocation,
    target_members: &[PoolMemberId],
) -> Option<PoolMemberId> {
    terminal_stored_member(location).filter(|member| target_members.contains(member))
}

fn catalog_state_label(location: &PoolCatalogLocation) -> &'static str {
    match location {
        PoolCatalogLocation::Missing => "missing",
        PoolCatalogLocation::Pending { .. } => "pending",
        PoolCatalogLocation::Stored { .. } => "stored",
        PoolCatalogLocation::Moving { .. } => "moving",
    }
}

fn catalog_candidates(location: &PoolCatalogLocation) -> Vec<PoolMemberId> {
    match location {
        PoolCatalogLocation::Missing => Vec::new(),
        PoolCatalogLocation::Pending { member, .. }
        | PoolCatalogLocation::Stored { member, .. } => vec![*member],
        PoolCatalogLocation::Moving { source, target, .. } => vec![*target, *source],
    }
}

fn empty_source_batch(hashes: &[Hash]) -> SourceBatch {
    SourceBatch {
        states: hashes
            .iter()
            .map(|hash| (*hash, SourceStatus::NotProbed))
            .collect(),
        valid_data: BTreeMap::new(),
    }
}

fn probe_source<E, R>(hashes: &[Hash], read_limit_bytes: u64, existing: E, read: R) -> SourceBatch
where
    E: FnOnce(&[Hash]) -> Result<Vec<bool>, hashtree_core::store::StoreError>,
    R: Fn(&[Hash], u64) -> Result<Vec<(Hash, Vec<u8>)>, hashtree_core::store::StoreError>,
{
    let present = match existing(hashes) {
        Ok(present) if present.len() == hashes.len() => present,
        Ok(present) => {
            let error = format!(
                "existence probe returned {} states for {} hashes",
                present.len(),
                hashes.len()
            );
            return source_error_batch(hashes, error);
        }
        Err(error) => return source_error_batch(hashes, error.to_string()),
    };
    let mut states = hashes
        .iter()
        .zip(&present)
        .map(|(hash, present)| {
            (
                *hash,
                if *present {
                    SourceStatus::Error("present body was not read".into())
                } else {
                    SourceStatus::Missing
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let present_hashes = hashes
        .iter()
        .zip(present)
        .filter_map(|(hash, present)| present.then_some(*hash))
        .collect::<Vec<_>>();
    let mut valid_data = BTreeMap::new();
    let mut offset = 0usize;
    while offset < present_hashes.len() {
        let bodies = match read(&present_hashes[offset..], read_limit_bytes) {
            Ok(bodies) if !bodies.is_empty() => bodies,
            Ok(_) => {
                let error = "bounded reader made no progress".to_string();
                for hash in &present_hashes[offset..] {
                    states.insert(*hash, SourceStatus::Error(error.clone()));
                }
                break;
            }
            Err(error) => {
                let error = error.to_string();
                for hash in &present_hashes[offset..] {
                    states.insert(*hash, SourceStatus::Error(error.clone()));
                }
                break;
            }
        };
        if bodies.len() > present_hashes.len() - offset {
            let error = "bounded reader returned more bodies than requested".to_string();
            for hash in &present_hashes[offset..] {
                states.insert(*hash, SourceStatus::Error(error.clone()));
            }
            break;
        }
        let body_count = bodies.len();
        for (body_index, (actual_hash, body)) in bodies.into_iter().enumerate() {
            let expected_hash = present_hashes[offset + body_index];
            if actual_hash != expected_hash {
                let error = "bounded reader returned hashes out of order".to_string();
                for hash in &present_hashes[offset..] {
                    states.insert(*hash, SourceStatus::Error(error.clone()));
                }
                return SourceBatch { states, valid_data };
            }
            if sha256(&body) == expected_hash {
                states.insert(expected_hash, SourceStatus::Valid);
                valid_data.insert(expected_hash, body);
            } else {
                states.insert(expected_hash, SourceStatus::Corrupt);
            }
        }
        offset += body_count;
    }
    SourceBatch { states, valid_data }
}

fn source_error_batch(hashes: &[Hash], error: String) -> SourceBatch {
    SourceBatch {
        states: hashes
            .iter()
            .map(|hash| (*hash, SourceStatus::Error(error.clone())))
            .collect(),
        valid_data: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn only_terminal_stored_catalog_state_is_target_residency() {
        let source =
            PoolMemberId::from_str("00000000-0000-0000-0000-000000000001").expect("source ID");
        let target =
            PoolMemberId::from_str("00000000-0000-0000-0000-000000000002").expect("target ID");
        let targets = [target];
        assert_eq!(
            terminal_target_member(
                &PoolCatalogLocation::Stored {
                    member: target,
                    size: 1,
                },
                &targets,
            ),
            Some(target)
        );
        assert_eq!(
            terminal_target_member(
                &PoolCatalogLocation::Pending {
                    member: target,
                    size: 1,
                },
                &targets,
            ),
            None
        );
        assert_eq!(
            terminal_target_member(
                &PoolCatalogLocation::Moving {
                    source,
                    target,
                    size: 1,
                },
                &targets,
            ),
            None
        );
        assert_eq!(
            terminal_target_member(
                &PoolCatalogLocation::Stored {
                    member: source,
                    size: 1,
                },
                &targets,
            ),
            None
        );
    }
}
