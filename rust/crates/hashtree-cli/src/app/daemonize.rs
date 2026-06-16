#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
#[cfg(unix)]
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::util::format_bytes;
#[cfg(unix)]
use super::util::process_is_running;

fn format_duration_compact(seconds: u64) -> String {
    if seconds >= 60 {
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    format!("{seconds}s")
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|value| value.as_u64())
}

fn append_queue_status(lines: &mut Vec<String>, queues: &serde_json::Value) {
    lines.push(String::new());
    lines.push("Queues:".to_string());

    if let Some(reads) = queues.get("blob_reads") {
        let in_use = json_u64(reads, "in_use").unwrap_or(0);
        let limit = json_u64(reads, "limit").unwrap_or(0);
        let available = json_u64(reads, "available").unwrap_or(0);
        let queue_timeout = json_u64(reads, "queue_timeout_ms").unwrap_or(0);
        let task_timeout = json_u64(reads, "task_timeout_ms").unwrap_or(0);
        lines.push(format!(
            "  Blob reads: {in_use}/{limit} in use, {available} available, queue {queue_timeout}ms, task {task_timeout}ms"
        ));
    }

    if let Some(writes) = queues.get("blob_writes") {
        let in_use = json_u64(writes, "in_use").unwrap_or(0);
        let limit = json_u64(writes, "limit").unwrap_or(0);
        let available = json_u64(writes, "available").unwrap_or(0);
        lines.push(format!(
            "  Blob writes: {in_use}/{limit} in use, {available} available"
        ));
    }

    if let Some(uploads) = queues.get("optimistic_uploads") {
        let enabled = uploads
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let reserved = json_u64(uploads, "reserved_bytes").unwrap_or(0);
        let max = json_u64(uploads, "max_bytes").unwrap_or(0);
        let in_flight = json_u64(uploads, "in_flight").unwrap_or(0);
        let queue_timeout = json_u64(uploads, "queue_timeout_ms").unwrap_or(0);
        lines.push(format!(
            "  Optimistic uploads: {}, {}/{} reserved, {} in flight, queue {}ms",
            if enabled { "enabled" } else { "disabled" },
            format_bytes(reserved),
            format_bytes(max),
            in_flight,
            queue_timeout
        ));
    }

    if let Some(replication) = queues.get("upload_replication") {
        let enabled = replication
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let targets = json_u64(replication, "targets").unwrap_or(0);
        let reserved = json_u64(replication, "reserved_bytes").unwrap_or(0);
        let max = json_u64(replication, "max_bytes").unwrap_or(0);
        let queued = json_u64(replication, "coalesce_queued_jobs").unwrap_or(0);
        let in_flight = json_u64(replication, "in_flight_batches").unwrap_or(0);
        let accepted_batches = json_u64(replication, "accepted_batches").unwrap_or(0);
        let accepted_blobs = json_u64(replication, "accepted_blobs").unwrap_or(0);
        let replicated_bytes = json_u64(replication, "replicated_bytes").unwrap_or(0);
        let failed_batches = json_u64(replication, "failed_batches").unwrap_or(0);
        let skipped_jobs = json_u64(replication, "skipped_jobs").unwrap_or(0);
        lines.push(format!(
            "  Upload replication: {}, {} target(s), {}/{} reserved, {} queued, {} in flight, accepted {} batch(es)/{} blob(s) ({}), failed {}, skipped {}",
            if enabled { "enabled" } else { "disabled" },
            targets,
            format_bytes(reserved),
            format_bytes(max),
            queued,
            in_flight,
            accepted_batches,
            accepted_blobs,
            format_bytes(replicated_bytes),
            failed_batches,
            skipped_jobs
        ));
    }
}

fn append_http_status(lines: &mut Vec<String>, status: &serde_json::Value) {
    let Some(classes) = status
        .get("http")
        .and_then(|http| http.get("status_classes"))
    else {
        return;
    };
    let Some(recent) = classes.get("recent") else {
        return;
    };

    let window = json_u64(classes, "window_seconds").unwrap_or(0);
    lines.push(String::new());
    lines.push("HTTP:".to_string());
    lines.push(format!(
        "  Last {}s: {} total, {} 1xx, {} 2xx, {} 3xx, {} 4xx, {} 5xx, {} other",
        window,
        json_u64(recent, "total").unwrap_or(0),
        json_u64(recent, "1xx").unwrap_or(0),
        json_u64(recent, "2xx").unwrap_or(0),
        json_u64(recent, "3xx").unwrap_or(0),
        json_u64(recent, "4xx").unwrap_or(0),
        json_u64(recent, "5xx").unwrap_or(0),
        json_u64(recent, "other").unwrap_or(0)
    ));
}

fn append_upstream_status(lines: &mut Vec<String>, status: &serde_json::Value) {
    let Some(upstream) = status.get("upstream") else {
        return;
    };

    let blossom_servers = json_u64(upstream, "blossom_servers").unwrap_or(0);
    let nostr_relays = json_u64(upstream, "nostr_relays").unwrap_or(0);
    let fetch = upstream.get("blossom_fetch");

    lines.push(String::new());
    lines.push("Upstream:".to_string());
    lines.push(format!(
        "  Blossom servers: {blossom_servers}, Nostr relays: {nostr_relays}"
    ));

    if let Some(fetch) = fetch {
        lines.push(format!(
            "  Blossom fetch: {} lookup(s), {} hit(s) ({}), {} explicit miss(es), {} indeterminate miss(es), {} cache hit(s)",
            json_u64(fetch, "lookup_attempts").unwrap_or(0),
            json_u64(fetch, "hits").unwrap_or(0),
            format_bytes(json_u64(fetch, "hit_bytes").unwrap_or(0)),
            json_u64(fetch, "explicit_misses").unwrap_or(0),
            json_u64(fetch, "indeterminate_misses").unwrap_or(0),
            json_u64(fetch, "miss_cache_hits").unwrap_or(0),
        ));
    }
}

pub(crate) fn format_daemon_status(status: &serde_json::Value, include_header: bool) -> String {
    let mut lines = Vec::new();
    if include_header {
        lines.push("Daemon Status:".to_string());
    }
    let status_text = status["status"].as_str().unwrap_or("unknown");
    lines.push(format!("  Status: {}", status_text));
    if let Some(uptime) = status
        .get("uptime_seconds")
        .and_then(|value| value.as_u64())
    {
        lines.push(format!("  Uptime: {}", format_duration_compact(uptime)));
    }
    if let Some(mode) = status.get("mode").and_then(|value| value.as_str()) {
        lines.push(format!("  Mode: {}", mode));
    }
    if let Some(hash_get) = status
        .get("capabilities")
        .and_then(|value| value.get("hash_get"))
        .and_then(|value| value.as_bool())
    {
        lines.push(format!(
            "  Hash Get: {}",
            if hash_get { "enabled" } else { "disabled" }
        ));
    }
    if let Some(http_webrtc_fetch) = status
        .get("capabilities")
        .and_then(|value| value.get("http_webrtc_fetch"))
        .and_then(|value| value.as_bool())
    {
        lines.push(format!(
            "  HTTP WebRTC fetch: {}",
            if http_webrtc_fetch {
                "enabled"
            } else {
                "disabled"
            }
        ));
    }

    if let Some(storage) = status.get("storage") {
        lines.push(String::new());
        lines.push("Storage:".to_string());
        if let Some(total) = storage.get("total_dags") {
            lines.push(format!("  Stored objects: {}", total));
        }
        if let Some(pinned) = storage.get("pinned_dags") {
            lines.push(format!("  Pinned items: {}", pinned));
        }
        if let Some(bytes) = storage.get("total_bytes").and_then(|b| b.as_u64()) {
            lines.push(format!("  Total size: {}", format_bytes(bytes)));
        }
    }

    if let Some(queues) = status.get("queues") {
        append_queue_status(&mut lines, queues);
    }
    append_upstream_status(&mut lines, status);
    append_http_status(&mut lines, status);

    if let Some(webrtc) = status.get("webrtc") {
        lines.push(String::new());
        lines.push("WebRTC:".to_string());
        if webrtc
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            lines.push("  Enabled: yes".to_string());
            if let Some(total) = webrtc.get("total_peers") {
                lines.push(format!("  Total peers: {}", total));
            }
            if let Some(connected) = webrtc.get("connected") {
                lines.push(format!("  Connected: {}", connected));
            }
            if let Some(dc) = webrtc.get("with_data_channel") {
                lines.push(format!("  With data channel: {}", dc));
            }
            if let Some(sent) = webrtc.get("bytes_sent").and_then(|b| b.as_u64()) {
                lines.push(format!("  Bytes sent: {}", format_bytes(sent)));
            }
            if let Some(received) = webrtc.get("bytes_received").and_then(|b| b.as_u64()) {
                lines.push(format!("  Bytes received: {}", format_bytes(received)));
            }
        } else {
            lines.push("  Enabled: no".to_string());
        }
    }

    if let Some(upstream) = status.get("upstream") {
        if let Some(count) = upstream.get("blossom_servers").and_then(|c| c.as_u64()) {
            if count > 0 {
                lines.push(String::new());
                lines.push("Upstream:".to_string());
                lines.push(format!("  Blossom servers: {}", count));
            }
        }
    }

    lines.join("\n")
}

#[cfg(unix)]
fn default_daemon_log_file() -> PathBuf {
    hashtree_cli::config::get_hashtree_dir()
        .join("logs")
        .join("htree.log")
}

fn default_daemon_pid_file() -> PathBuf {
    hashtree_cli::config::get_hashtree_dir().join("htree.pid")
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonLaunchState {
    pub(crate) addr: Option<String>,
    pub(crate) relays: Option<String>,
    pub(crate) mode: Option<hashtree_cli::config::ServerMode>,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) log_file: PathBuf,
}

#[cfg(unix)]
pub(crate) fn daemon_state_file_path(pid_file: &std::path::Path) -> PathBuf {
    pid_file.with_extension("daemon.toml")
}

#[cfg(unix)]
pub(crate) fn read_daemon_launch_state(path: &std::path::Path) -> Result<DaemonLaunchState> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read daemon state file {}", path.display()))?;
    toml::from_str(&raw)
        .with_context(|| format!("Failed to parse daemon state file {}", path.display()))
}

#[cfg(unix)]
pub(crate) fn write_daemon_launch_state(
    path: &std::path::Path,
    state: &DaemonLaunchState,
) -> Result<()> {
    let raw = toml::to_string(state).context("Failed to serialize daemon state")?;
    std::fs::write(path, raw)
        .with_context(|| format!("Failed to write daemon state file {}", path.display()))
}

#[cfg(unix)]
pub(crate) fn build_daemon_args(
    addr: Option<&str>,
    relays: Option<&str>,
    mode: Option<hashtree_cli::config::ServerMode>,
    data_dir: Option<&PathBuf>,
) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    if let Some(addr) = addr {
        args.push(std::ffi::OsString::from("--addr"));
        args.push(std::ffi::OsString::from(addr));
    }
    if let Some(relays) = relays {
        args.push(std::ffi::OsString::from("--relays"));
        args.push(std::ffi::OsString::from(relays));
    }
    if let Some(mode) = mode {
        args.push(std::ffi::OsString::from("--mode"));
        args.push(std::ffi::OsString::from(mode.as_str()));
    }
    if let Some(data_dir) = data_dir {
        args.push(std::ffi::OsString::from("--data-dir"));
        args.push(data_dir.as_os_str().to_owned());
    }
    args
}

pub(crate) fn spawn_daemon(
    addr: Option<&str>,
    relays: Option<&str>,
    mode: Option<hashtree_cli::config::ServerMode>,
    data_dir: Option<PathBuf>,
    log_file: Option<&PathBuf>,
    pid_file: Option<&PathBuf>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::{self, OpenOptions};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let log_path = log_file.cloned().unwrap_or_else(default_daemon_log_file);
        let pid_path = pid_file.cloned().unwrap_or_else(default_daemon_pid_file);
        let state_path = daemon_state_file_path(&pid_path);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create log dir {}", parent.display()))?;
        }
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create pid dir {}", parent.display()))?;
        }
        if let Some(parent) = state_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create state dir {}", parent.display()))?;
        }

        if pid_path.exists() {
            let pid = read_pid_file(&pid_path)
                .with_context(|| format!("Failed to read pid file {}", pid_path.display()))?;
            if is_process_running(pid) {
                anyhow::bail!("Daemon already running (pid {})", pid);
            }
            fs::remove_file(&pid_path).with_context(|| {
                format!("Failed to remove stale pid file {}", pid_path.display())
            })?;
        }

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to open log file {}", log_path.display()))?;
        let log_err = log.try_clone().context("Failed to clone log file handle")?;

        let launch_state = DaemonLaunchState {
            addr: addr.map(ToOwned::to_owned),
            relays: relays.map(ToOwned::to_owned),
            mode,
            data_dir,
            log_file: log_path.clone(),
        };
        write_daemon_launch_state(&state_path, &launch_state)?;

        let exe = std::env::current_exe().context("Failed to locate htree binary")?;
        let mut cmd = Command::new(exe);
        cmd.arg("start")
            .args(build_daemon_args(
                launch_state.addr.as_deref(),
                launch_state.relays.as_deref(),
                launch_state.mode,
                launch_state.data_dir.as_ref(),
            ))
            .env("HTREE_DAEMONIZED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));

        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().context("Failed to spawn daemon")?;
        write_pid_file(&pid_path, child.id())
            .with_context(|| format!("Failed to write pid file {}", pid_path.display()))?;
        println!("Started hashtree daemon (pid {})", child.id());
        println!("Log file: {}", log_path.display());
        println!("PID file: {}", pid_path.display());
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = addr;
        let _ = relays;
        let _ = mode;
        let _ = data_dir;
        let _ = log_file;
        let _ = pid_file;
        anyhow::bail!("Daemon mode is only supported on Unix systems");
    }
}

#[cfg(unix)]
pub(crate) fn parse_pid(contents: &str) -> Result<i32> {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        anyhow::bail!("PID file is empty");
    }
    let pid: i32 = trimmed.parse().context("Invalid PID value")?;
    if pid <= 0 {
        anyhow::bail!("PID must be a positive integer");
    }
    Ok(pid)
}

#[cfg(unix)]
pub(crate) fn read_pid_file(path: &std::path::Path) -> Result<i32> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read pid file {}", path.display()))?;
    parse_pid(&contents)
}

#[cfg(unix)]
pub(crate) fn write_pid_file(path: &std::path::Path, pid: u32) -> Result<()> {
    std::fs::write(path, format!("{}\n", pid))
        .with_context(|| format!("Failed to write pid file {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn is_process_running(pid: i32) -> bool {
    process_is_running(pid as u32)
}

#[cfg(unix)]
fn signal_process(pid: i32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    anyhow::bail!("Failed to signal pid {}: {}", pid, err);
}

pub(crate) fn stop_daemon(pid_file: Option<&PathBuf>) -> Result<()> {
    let pid_path = pid_file.cloned().unwrap_or_else(default_daemon_pid_file);

    #[cfg(unix)]
    {
        let pid = read_pid_file(&pid_path)?;
        if !is_process_running(pid) {
            let _ = std::fs::remove_file(&pid_path);
            anyhow::bail!("Daemon not running (pid {})", pid);
        }

        signal_process(pid, libc::SIGTERM)?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !is_process_running(pid) {
                std::fs::remove_file(&pid_path)
                    .with_context(|| format!("Failed to remove pid file {}", pid_path.display()))?;
                println!("Stopped hashtree daemon (pid {})", pid);
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        anyhow::bail!("Timed out waiting for daemon to stop (pid {})", pid);
    }

    #[cfg(not(unix))]
    {
        let _ = pid_path;
        anyhow::bail!("Daemon stop is only supported on Unix systems");
    }
}

pub(crate) fn reload_daemon(pid_file: Option<&PathBuf>) -> Result<()> {
    let pid_path = pid_file.cloned().unwrap_or_else(default_daemon_pid_file);

    #[cfg(unix)]
    {
        let state_path = daemon_state_file_path(&pid_path);
        let state = read_daemon_launch_state(&state_path).with_context(|| {
            format!(
                "Reload needs daemon launch state. Start the daemon with `htree start --daemon` so {} exists",
                state_path.display()
            )
        })?;
        let pid = read_pid_file(&pid_path)?;
        println!("Reloading hashtree daemon (pid {})", pid);
        stop_daemon(Some(&pid_path))?;
        spawn_daemon(
            state.addr.as_deref(),
            state.relays.as_deref(),
            state.mode,
            state.data_dir,
            Some(&state.log_file),
            Some(&pid_path),
        )?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = pid_path;
        anyhow::bail!("Daemon reload is only supported on Unix systems");
    }
}
