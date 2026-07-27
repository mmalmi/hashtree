use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CONFIG_SCHEMA: &str = "iris-audio-target-pool-residency-audit-config/v1";
pub const CHECKPOINT_SCHEMA: &str = "iris-audio-target-pool-residency-audit-checkpoint/v2";
pub const LEDGER_ROW_SCHEMA: &str = "iris-audio-target-pool-residency-block/v2";
pub const MANIFEST_SCHEMA: &str = "iris-audio-target-pool-residency-manifest/v1";
pub const INVENTORY_IDENTITY_SCHEMA: &str = "iris-audio-target-pool-inventory-identity/v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditConfig {
    pub schema: String,
    pub pool_catalog: std::path::PathBuf,
    pub expected_pool_members: Vec<String>,
    pub target_members: Vec<String>,
    #[serde(default)]
    pub fallback_tiers: Vec<FallbackTierConfig>,
    pub expected_inventory_sha256: String,
    pub expected_inventory_records: usize,
    #[serde(default)]
    pub additional_roots: Vec<AdditionalRootConfig>,
    #[serde(default = "default_work_item_batch_size")]
    pub work_item_batch_size: usize,
    #[serde(default = "default_read_limit_bytes")]
    pub read_limit_bytes: u64,
}

fn default_work_item_batch_size() -> usize {
    256
}

fn default_read_limit_bytes() -> u64 {
    128 * 1024 * 1024
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FallbackTierConfig {
    pub name: String,
    pub lmdb_path: std::path::PathBuf,
    #[serde(default)]
    pub external_blob_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdditionalRootConfig {
    pub id: String,
    pub role: String,
    pub hash: String,
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InventoryRow {
    pub source_key: String,
    pub song_id: String,
    pub hash: hashtree_core::Hash,
    pub key: [u8; 32],
    pub input_line: usize,
}

#[derive(Debug, Clone)]
pub struct WorkItem {
    pub kind: &'static str,
    pub id: String,
    pub source_key: Option<String>,
    pub song_id: Option<String>,
    pub input_line: Option<usize>,
    pub role: String,
    pub hash: hashtree_core::Hash,
    pub key: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditSummary {
    pub work_items_processed: u64,
    pub inventory_records_processed: u64,
    pub additional_roots_processed: u64,
    pub complete_work_items: u64,
    pub incomplete_work_items: u64,
    pub block_references: u64,
    pub target_valid: u64,
    pub fallback_only: u64,
    pub catalog_mismatch: u64,
    pub missing: u64,
    pub corrupt: u64,
    pub unknown: u64,
    pub traversal_failures: u64,
    pub discovered_external_roots: u64,
    pub role_counts: BTreeMap<String, u64>,
}

impl AuditSummary {
    pub fn observe_residency(&mut self, residency: &str) -> Result<(), String> {
        match residency {
            "target-valid" => self.target_valid += 1,
            "fallback-only" => self.fallback_only += 1,
            "catalog-mismatch" => self.catalog_mismatch += 1,
            "missing" => self.missing += 1,
            "corrupt" => self.corrupt += 1,
            "unknown" => self.unknown += 1,
            other => return Err(format!("unhandled residency classification {other}")),
        }
        Ok(())
    }

    pub fn release_ready(&self, total_work_items: usize) -> bool {
        self.work_items_processed == total_work_items as u64
            && self.incomplete_work_items == 0
            && self.fallback_only == 0
            && self.catalog_mismatch == 0
            && self.missing == 0
            && self.corrupt == 0
            && self.unknown == 0
            && self.traversal_failures == 0
    }

    pub fn validate_checkpoint(
        &self,
        next_work_item: usize,
        inventory_records: usize,
    ) -> Result<(), String> {
        let processed = next_work_item as u64;
        let expected_inventory = next_work_item.min(inventory_records) as u64;
        let expected_additional = next_work_item.saturating_sub(inventory_records) as u64;
        if self.work_items_processed != processed
            || self.inventory_records_processed != expected_inventory
            || self.additional_roots_processed != expected_additional
            || self.complete_work_items + self.incomplete_work_items != processed
        {
            return Err("work-item counters do not match the durable cursor".into());
        }
        let classified = self.target_valid
            + self.fallback_only
            + self.catalog_mismatch
            + self.missing
            + self.corrupt
            + self.unknown;
        if classified != self.block_references {
            return Err("residency counters do not match blockReferences".into());
        }
        if self.role_counts.values().sum::<u64>() != self.block_references {
            return Err("role counters do not match blockReferences".into());
        }
        if self.traversal_failures > self.block_references {
            return Err("traversalFailures exceeds blockReferences".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub schema: String,
    pub config_sha256: String,
    pub pool_manifest_sha256: String,
    pub pool_manifest_generation: u64,
    pub inventory_sha256: String,
    pub inventory_records: usize,
    pub inventory_identity_sha256: String,
    pub next_work_item: usize,
    pub ledger_bytes: u64,
    pub ledger_sha256: String,
    pub summary: AuditSummary,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockLedgerRow {
    pub schema: &'static str,
    pub work_item_index: usize,
    pub work_item_kind: &'static str,
    pub work_item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_line: Option<usize>,
    pub root_hash: String,
    pub block_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub path: String,
    pub role: String,
    pub expected_link_type: String,
    pub catalog_state: String,
    pub catalog_candidates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_declared_size: Option<u64>,
    pub catalog_target_membership: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_error: Option<String>,
    pub target_members: BTreeMap<String, String>,
    pub fallback_tiers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_witness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_witness: Option<String>,
    pub residency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_bytes: Option<usize>,
    pub traversal: String,
    pub discovered_external_roots: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryIdentityManifest {
    pub schema: &'static str,
    pub sha256: String,
    pub records: usize,
    pub identity_sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetManifest {
    pub pool_manifest_sha256: String,
    pub pool_manifest_generation: u64,
    pub expected_pool_member_ids: Vec<String>,
    pub target_member_ids: Vec<String>,
    pub fallback_tier_names: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerManifest {
    pub schema: &'static str,
    pub sha256: String,
    pub bytes: u64,
    pub rows: u64,
    pub unique_block_hashes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditManifest {
    pub schema: &'static str,
    pub config_sha256: String,
    pub inventory: InventoryIdentityManifest,
    pub target: TargetManifest,
    pub work_items: usize,
    pub ledger: LedgerManifest,
    pub summary: AuditSummary,
    pub release_ready: bool,
}

#[derive(Debug, Clone)]
pub struct BlockRef {
    pub hash: hashtree_core::Hash,
    pub key: Option<[u8; 32]>,
    pub path: String,
    pub role: String,
    pub expected_link_type: Option<hashtree_core::LinkType>,
}

#[derive(Debug)]
pub struct RunOutcome {
    pub next_work_item: usize,
    pub total_work_items: usize,
    pub ledger_rows: u64,
    pub complete: bool,
    pub release_ready: bool,
}
