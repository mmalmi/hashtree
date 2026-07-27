use crate::model::{
    AdditionalRootConfig, AuditConfig, FallbackTierConfig, CONFIG_SCHEMA, INVENTORY_IDENTITY_SCHEMA,
};
use hashtree_core::{from_hex, sha256, to_hex, Hash};
use hashtree_lmdb::PoolMemberId;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::str::FromStr;

pub struct ValidatedConfig {
    pub config: AuditConfig,
    pub config_sha256: String,
    pub expected_pool_members: Vec<PoolMemberId>,
    pub target_members: Vec<PoolMemberId>,
}

pub fn load_config(path: &Path) -> Result<ValidatedConfig, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    let config_sha256 = to_hex(&sha256(&bytes));
    let config: AuditConfig = serde_json::from_slice(&bytes)?;
    validate_config(&config)?;
    let mut expected_pool_members = config
        .expected_pool_members
        .iter()
        .map(|value| PoolMemberId::from_str(value))
        .collect::<Result<Vec<_>, _>>()?;
    expected_pool_members.sort_unstable();
    let mut target_members = config
        .target_members
        .iter()
        .map(|value| PoolMemberId::from_str(value))
        .collect::<Result<Vec<_>, _>>()?;
    target_members.sort_unstable();
    Ok(ValidatedConfig {
        config,
        config_sha256,
        expected_pool_members,
        target_members,
    })
}

fn validate_config(config: &AuditConfig) -> Result<(), Box<dyn std::error::Error>> {
    if config.schema != CONFIG_SCHEMA {
        return Err(format!(
            "unsupported config schema {}; expected {CONFIG_SCHEMA}",
            config.schema
        )
        .into());
    }
    if config.expected_pool_members.is_empty() {
        return Err("config expectedPoolMembers must not be empty".into());
    }
    if config.target_members.is_empty() {
        return Err("config targetMembers must not be empty".into());
    }
    if config.expected_inventory_records == 0 {
        return Err("config expectedInventoryRecords must be greater than zero".into());
    }
    validate_sha256_hex("expectedInventorySha256", &config.expected_inventory_sha256)?;
    if config.work_item_batch_size == 0 {
        return Err("config workItemBatchSize must be greater than zero".into());
    }
    if config.read_limit_bytes == 0 {
        return Err("config readLimitBytes must be greater than zero".into());
    }

    let mut expected_member_ids = BTreeSet::new();
    for value in &config.expected_pool_members {
        let id = PoolMemberId::from_str(value)?;
        if !expected_member_ids.insert(id) {
            return Err(format!("duplicate expected Pool member {id}").into());
        }
    }
    let mut target_member_ids = BTreeSet::new();
    for value in &config.target_members {
        let id = PoolMemberId::from_str(value)?;
        if !target_member_ids.insert(id) {
            return Err(format!("duplicate target member {id}").into());
        }
        if !expected_member_ids.contains(&id) {
            return Err(format!("target member {id} is not present in expectedPoolMembers").into());
        }
    }
    let mut fallback_names = BTreeSet::new();
    for tier in &config.fallback_tiers {
        validate_fallback(tier)?;
        if !fallback_names.insert(tier.name.as_str()) {
            return Err(format!("duplicate fallback tier {}", tier.name).into());
        }
    }
    let mut additional_ids = BTreeSet::new();
    for root in &config.additional_roots {
        validate_additional_root(root)?;
        if !additional_ids.insert(root.id.as_str()) {
            return Err(format!("duplicate additional root id {}", root.id).into());
        }
    }
    if !config
        .additional_roots
        .iter()
        .any(|root| root.role == "catalog")
    {
        return Err("config additionalRoots must pin at least one catalog root".into());
    }
    Ok(())
}

fn validate_fallback(tier: &FallbackTierConfig) -> Result<(), Box<dyn std::error::Error>> {
    if tier.name.trim().is_empty() || tier.name != tier.name.trim() {
        return Err("fallback tier names must be non-empty and trimmed".into());
    }
    if tier.name.contains(['\n', '\r', '\t']) {
        return Err(format!("invalid fallback tier name {:?}", tier.name).into());
    }
    Ok(())
}

fn validate_additional_root(root: &AdditionalRootConfig) -> Result<(), Box<dyn std::error::Error>> {
    if root.id.trim().is_empty() || root.id != root.id.trim() {
        return Err("additional root ids must be non-empty and trimmed".into());
    }
    if !matches!(root.role.as_str(), "catalog" | "song" | "audio" | "image") {
        return Err(format!(
            "additional root {} has unsupported role {}",
            root.id, root.role
        )
        .into());
    }
    canonical_hash("additional root hash", &root.hash)?;
    if let Some(key) = &root.key {
        canonical_hash("additional root key", key)?;
    }
    Ok(())
}

pub fn canonical_hash(label: &str, value: &str) -> Result<Hash, Box<dyn std::error::Error>> {
    let hash = from_hex(value)?;
    if to_hex(&hash) != value {
        return Err(format!("{label} is not canonical lowercase SHA256 hex").into());
    }
    Ok(hash)
}

fn validate_sha256_hex(label: &str, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    canonical_hash(label, value)?;
    Ok(())
}

pub fn inventory_identity_sha256(inventory_sha256: &str, records: usize) -> String {
    let canonical =
        format!("{INVENTORY_IDENTITY_SCHEMA}\nsha256={inventory_sha256}\nrecords={records}\n");
    to_hex(&sha256(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_identity_pins_hash_and_record_count() {
        let first = inventory_identity_sha256(&"00".repeat(32), 185_730);
        assert_ne!(first, inventory_identity_sha256(&"00".repeat(32), 185_729));
        assert_ne!(first, inventory_identity_sha256(&"01".repeat(32), 185_730));
    }
}
