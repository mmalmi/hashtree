use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

pub(super) const CHECKPOINT_REQUEST_SCHEMA: &str = "hashtree-pool-migration-checkpoint-request/v3";
pub(super) const CHECKPOINT_ACK_SCHEMA: &str = "hashtree-pool-migration-checkpoint-ack/v3";
pub(super) const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CheckpointBrokerAuthorityV3 {
    pub(super) pid: u32,
    pub(super) proc_start_time_ticks: u64,
    pub(super) timeout_seconds: u64,
    pub(super) systemd_unit: String,
    pub(super) systemd_invocation_id: String,
    pub(super) systemd_fragment_path: PathBuf,
    pub(super) systemd_fragment_sha256: String,
    pub(super) systemd_environment_file_path: PathBuf,
    pub(super) systemd_environment_file_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MigrationCheckpointRequestV3 {
    pub(super) schema: String,
    pub(super) sequence: u64,
    pub(super) previous_ack_sha256: Option<String>,
    pub(super) operation: String,
    pub(super) cursor: Option<String>,
    pub(super) range_limit: Option<u64>,
    pub(super) worker_pid: u32,
    pub(super) worker_proc_start_time_ticks: u64,
    pub(super) broker_pid: u32,
    pub(super) broker_proc_start_time_ticks: u64,
    pub(super) boot_id: String,
    pub(super) attempt_nonce: String,
    pub(super) launch_request_sha256: String,
    pub(super) requested_at_boottime_millis: u64,
    pub(super) start_before_boottime_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MigrationCheckpointAckV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) sequence: u64,
    pub(super) previous_ack_sha256: Option<String>,
    pub(super) request_sha256: String,
    pub(super) operation: String,
    pub(super) cursor: Option<String>,
    pub(super) range_limit: Option<u64>,
    pub(super) worker_pid: u32,
    pub(super) worker_proc_start_time_ticks: u64,
    pub(super) broker_pid: u32,
    pub(super) broker_proc_start_time_ticks: u64,
    pub(super) boot_id: String,
    pub(super) attempt_nonce: String,
    pub(super) launch_request_sha256: String,
    pub(super) authorized_at_boottime_millis: u64,
    pub(super) start_before_boottime_millis: u64,
}

pub(super) fn request_file_name(sequence: u64) -> String {
    format!("checkpoint-request-{sequence:020}.json")
}

pub(super) fn ack_file_name(sequence: u64) -> String {
    format!("checkpoint-ack-{sequence:020}.json")
}

pub(super) fn validate_checkpoint_operation(value: &str) -> Result<()> {
    if !matches!(
        value,
        "migration-batch"
            | "online-audit-batch"
            | "online-evidence-publication"
            | "online-audit-publication"
            | "online-readiness"
            | "source-keyset-audit"
            | "source-evidence-publication"
            | "source-evidence-consumed"
            | "source-generation-fingerprint"
            | "source-terminal-publication"
            | "target-terminal-audit"
            | "terminal-receipt-publication"
            | "terminal-readiness"
    ) {
        bail!("unsupported Pool migration checkpoint operation {value}");
    }
    Ok(())
}

pub(super) fn boottime_millis() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut value = std::mem::MaybeUninit::<libc::timespec>::zeroed();
        let status = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, value.as_mut_ptr()) };
        if status != 0 {
            return Err(std::io::Error::last_os_error()).context("read CLOCK_BOOTTIME");
        }
        let value = unsafe { value.assume_init() };
        let seconds =
            u64::try_from(value.tv_sec).context("CLOCK_BOOTTIME returned negative seconds")?;
        let nanos =
            u64::try_from(value.tv_nsec).context("CLOCK_BOOTTIME returned negative nanoseconds")?;
        seconds
            .checked_mul(1000)
            .and_then(|millis| millis.checked_add(nanos / 1_000_000))
            .context("CLOCK_BOOTTIME millisecond overflow")
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(0)
    }
}

pub(super) fn timeout_millis(timeout: Duration) -> Result<u64> {
    u64::try_from(timeout.as_millis()).context("checkpoint timeout is too large")
}

#[cfg(target_os = "linux")]
pub(super) fn validate_root_broker_process(pid: u32, expected_start_time_ticks: u64) -> Result<()> {
    if pid == 0 || expected_start_time_ticks == 0 {
        bail!("checkpoint broker process identity must be positive");
    }
    let proc_path = format!("/proc/{pid}");
    let metadata = std::fs::metadata(&proc_path)
        .with_context(|| format!("inspect checkpoint broker {proc_path}"))?;
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != 0 {
        bail!("checkpoint broker PID {pid} is not root-owned");
    }
    let stat_path = format!("{proc_path}/stat");
    let stat = std::fs::read_to_string(&stat_path)
        .with_context(|| format!("read checkpoint broker {stat_path}"))?;
    let command_end = stat
        .rfind(") ")
        .with_context(|| format!("parse checkpoint broker {stat_path}"))?;
    let actual = stat[command_end + 2..]
        .split_ascii_whitespace()
        .nth(19)
        .context("checkpoint broker stat has no starttime field")?
        .parse::<u64>()
        .context("parse checkpoint broker starttime")?;
    if actual != expected_start_time_ticks {
        bail!("checkpoint broker PID/starttime identity changed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn validate_root_broker_service(authority: &CheckpointBrokerAuthorityV3) -> Result<()> {
    validate_root_broker_process(authority.pid, authority.proc_start_time_ticks)?;
    let systemctl = if PathBuf::from("/usr/bin/systemctl").is_file() {
        "/usr/bin/systemctl"
    } else {
        "/bin/systemctl"
    };
    let output = std::process::Command::new(systemctl)
        .env_clear()
        .env("LANG", "C")
        .args([
            "--system",
            "--no-pager",
            "show",
            &authority.systemd_unit,
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=InvocationID",
            "--property=MainPID",
            "--property=ControlPID",
            "--property=NRestarts",
            "--property=Job",
            "--property=FragmentPath",
            "--property=EnvironmentFiles",
            "--property=NeedDaemonReload",
            "--property=Type",
            "--property=Restart",
        ])
        .output()
        .context("query dedicated root checkpoint broker service")?;
    if !output.status.success() {
        bail!(
            "systemd could not verify dedicated root checkpoint broker service: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8(output.stdout)
        .context("checkpoint broker systemd properties are not UTF-8")?;
    let mut properties = std::collections::HashMap::new();
    for line in text.lines() {
        let (name, value) = line
            .split_once('=')
            .context("checkpoint broker systemd returned malformed properties")?;
        if properties.insert(name, value).is_some() {
            bail!("checkpoint broker systemd returned duplicate property {name}");
        }
    }
    let property = |name| {
        properties
            .get(name)
            .copied()
            .with_context(|| format!("checkpoint broker systemd omitted {name}"))
    };
    let environment = authority
        .systemd_environment_file_path
        .to_str()
        .context("checkpoint broker environment path is not UTF-8")?;
    if property("LoadState")? != "loaded"
        || property("ActiveState")? != "active"
        || property("SubState")? != "running"
        || property("InvocationID")? != authority.systemd_invocation_id
        || property("MainPID")? != authority.pid.to_string()
        || property("ControlPID")? != "0"
        || property("NRestarts")? != "0"
        || !property("Job")?.is_empty()
        || property("FragmentPath")?
            != authority
                .systemd_fragment_path
                .to_str()
                .context("checkpoint broker fragment path is not UTF-8")?
        || !matches!(property("NeedDaemonReload")?, "no")
        || property("Type")? != "exec"
        || property("Restart")? != "no"
        || !matches!(
            property("EnvironmentFiles")?,
            value if value == environment
                || value == format!("{environment} (ignore_errors=no)")
        )
    {
        bail!("dedicated root checkpoint broker systemd authority changed or became inactive");
    }
    use sha2::{Digest, Sha256};
    for (path, expected, label) in [
        (
            &authority.systemd_fragment_path,
            &authority.systemd_fragment_sha256,
            "checkpoint broker fragment",
        ),
        (
            &authority.systemd_environment_file_path,
            &authority.systemd_environment_file_sha256,
            "checkpoint broker environment file",
        ),
    ] {
        let bytes = std::fs::read(path).with_context(|| format!("read {label}"))?;
        if hex::encode(Sha256::digest(bytes)) != *expected {
            bail!("{label} SHA-256 changed");
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_root_broker_process(
    _pid: u32,
    _expected_start_time_ticks: u64,
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_root_broker_service(_authority: &CheckpointBrokerAuthorityV3) -> Result<()> {
    Ok(())
}
