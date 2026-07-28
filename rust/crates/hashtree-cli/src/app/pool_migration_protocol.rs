use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::pool_migration_checkpoint::CheckpointBrokerAuthorityV3;
use super::pool_migration_mount::{ExecutionNamespaceAuthorityV3, SourceReadOnlyMountAuthorityV3};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolMigrationLaunchRequestV3 {
    pub(super) schema: String,
    pub(super) attempt_namespace: PathBuf,
    pub(super) attempt_namespace_identity: FileIdentityV3,
    pub(super) attempt_identity: FileIdentityV3,
    pub(super) nonce: String,
    pub(super) boot_id: String,
    pub(super) execution_namespaces: ExecutionNamespaceAuthorityV3,
    pub(super) systemd_invocation_id: String,
    pub(super) systemd_unit: String,
    pub(super) systemd_manager: String,
    pub(super) systemd_fragment: FileAuthorityV3,
    pub(super) systemd_environment_file: FileAuthorityV3,
    pub(super) main_pid: u32,
    pub(super) proc_start_time_ticks: u64,
    pub(super) binary: FileAuthorityV3,
    pub(super) argv: Vec<String>,
    pub(super) controller: ControllerAuthorityV3,
    pub(super) checkpoint_broker: CheckpointBrokerAuthorityV3,
    pub(super) source: SourceAuthorityV3,
    pub(super) pool: PoolAuthorityV3,
    pub(super) cursor: CursorAuthorityV3,
    pub(super) cas: Vec<NamedFileAuthorityV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FileAuthorityV3 {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FileIdentityV3 {
    pub(super) device: u64,
    pub(super) inode: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LmdbIdentityV3 {
    pub(super) directory: FileIdentityV3,
    pub(super) data: FileIdentityV3,
    pub(super) lock: FileIdentityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NamedFileAuthorityV3 {
    pub(super) label: String,
    pub(super) path: PathBuf,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ControllerAuthorityV3 {
    pub(super) rollout_id: String,
    pub(super) phase: String,
    pub(super) executable: FileAuthorityV3,
    pub(super) state: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ControllerStateV3 {
    pub(super) schema: String,
    pub(super) rollout_id: String,
    pub(super) phase: String,
    pub(super) boot_id: String,
    pub(super) source_lmdb_identity: LmdbIdentityV3,
    pub(super) source_external_identity: Option<FileIdentityV3>,
    pub(super) pool_lmdb_identity: LmdbIdentityV3,
    pub(super) pool_manifest_sha256: String,
    pub(super) pool_topology_sha256: String,
    pub(super) source_writers_fenced: bool,
    pub(super) target_writers_fenced: bool,
    pub(super) fence_held_until_completion: bool,
    pub(super) source_writer_processes_with_open_handles: u64,
    pub(super) target_writer_processes_with_open_handles: u64,
    pub(super) stopped_writer_units: Vec<String>,
    pub(super) writer_unit_masks: Vec<WriterUnitMaskV3>,
    pub(super) legacy_worker_template_mask: WriterUnitMaskV3,
    pub(super) legacy_worker_instance_masks: Vec<WriterUnitMaskV3>,
    pub(super) source_terminal_receipt_sha256: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WriterUnitMaskV3 {
    pub(super) unit: String,
    pub(super) path: PathBuf,
    pub(super) identity: FileIdentityV3,
    pub(super) target: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceAuthorityV3 {
    pub(super) lmdb_path: PathBuf,
    pub(super) lmdb_identity: LmdbIdentityV3,
    pub(super) external_path: Option<PathBuf>,
    pub(super) external_identity: Option<FileIdentityV3>,
    pub(super) read_only_mounts: Option<SourceReadOnlyMountAuthorityV3>,
    pub(super) baseline: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolAuthorityV3 {
    pub(super) path: PathBuf,
    pub(super) lmdb_identity: LmdbIdentityV3,
    pub(super) topology: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolTopologyV3 {
    pub(super) schema: String,
    pub(super) pool_path: PathBuf,
    pub(super) manifest_sha256: String,
    pub(super) members: Vec<PoolTopologyMemberV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PoolTopologyMemberV3 {
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) directory_identity: FileIdentityV3,
    pub(super) lmdb_identity: LmdbIdentityV3,
    pub(super) marker: FileAuthorityV3,
    pub(super) external_path: Option<PathBuf>,
    pub(super) external_directory_identity: Option<FileIdentityV3>,
    pub(super) external_marker: Option<FileAuthorityV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CursorAuthorityV3 {
    pub(super) path: PathBuf,
    pub(super) parent_identity: FileIdentityV3,
    pub(super) exists: bool,
    pub(super) value: Option<String>,
    pub(super) sha256: Option<String>,
}
