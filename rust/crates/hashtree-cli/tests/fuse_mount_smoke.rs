#![cfg(feature = "fuse")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output};
use std::time::{Duration, Instant};

use common::htree_bin;
use hashtree_cli::HashtreeStore;
use hashtree_core::{from_hex, nhash_encode};
use serde_json::Value;
use tempfile::TempDir;

const EMPTY_MOUNTS_OUTPUT: &str = "No active hashtree mounts found.";

struct ChildGuard {
    child: Option<Child>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl ChildGuard {
    fn spawn(command: &mut Command, output_dir: &Path) -> Self {
        let stdout_path = output_dir.join("mount.stdout");
        let stderr_path = output_dir.join("mount.stderr");
        command.stdout(std::fs::File::create(&stdout_path).expect("create mount stdout capture"));
        command.stderr(std::fs::File::create(&stderr_path).expect("create mount stderr capture"));
        let child = command.spawn().expect("spawn mount command");
        Self {
            child: Some(child),
            stdout_path,
            stderr_path,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child process available")
    }

    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }

    fn wait_with_output(&mut self) -> Output {
        let status = self
            .child
            .take()
            .expect("child process available")
            .wait()
            .expect("wait for child");
        self.output_with_status(status)
    }

    fn output_with_status(&self, status: ExitStatus) -> Output {
        Output {
            status,
            stdout: std::fs::read(&self.stdout_path).expect("read mount stdout capture"),
            stderr: std::fs::read(&self.stderr_path).expect("read mount stderr capture"),
        }
    }

    fn stop_after_unmount(&mut self, mountpoint: &Path) -> Output {
        if mountpoint_is_active(mountpoint) {
            let _ = try_unmount(mountpoint);
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self
                .child_mut()
                .try_wait()
                .expect("poll child after unmount")
                .is_some()
            {
                return self.wait_with_output();
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        self.kill();
        self.wait_with_output()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn mounts_command_reports_empty_state() {
    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let output = run_htree_command(&config_dir, &data_dir, ["mounts"]);
    assert_success(&output, "htree mounts");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains(EMPTY_MOUNTS_OUTPUT),
        "expected empty mounts output.\nstdout:\n{stdout}"
    );
}

#[test]
fn mount_smoke_lists_active_mount_and_unmounts_cleanly() {
    if let Some(reason) = fuse_smoke_skip_reason() {
        eprintln!("skipping fuse smoke test: {reason}");
        return;
    }

    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let source_dir = tmp.path().join("source");
    let mountpoint = tmp.path().join("mount");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::create_dir_all(source_dir.join("nested")).expect("create source dir");
    std::fs::create_dir_all(&mountpoint).expect("create mountpoint");
    std::fs::write(
        source_dir.join("nested").join("hello.txt"),
        "hello through fuse",
    )
    .expect("write source file");

    let store = HashtreeStore::new(&data_dir).expect("create store");
    let root_hex = store.upload_dir(&source_dir).expect("upload source dir");
    let nhash =
        nhash_encode(&from_hex(&root_hex).expect("decode root hash")).expect("encode nhash");

    let mut mount_command = Command::new(htree_bin());
    mount_command
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("mount")
        .arg(&nhash)
        .arg(&mountpoint);
    #[cfg(target_os = "macos")]
    mount_command.arg("--allow-other");
    mount_command.env("HTREE_CONFIG_DIR", &config_dir);

    let mut child = ChildGuard::spawn(&mut mount_command, &config_dir);
    wait_for_mounted_file(
        &mut child,
        &config_dir,
        &data_dir,
        &mountpoint,
        mountpoint.join("nested").join("hello.txt"),
        "hello through fuse",
    );

    let mounts_output = run_htree_command(&config_dir, &data_dir, ["mounts"]);
    assert_success(&mounts_output, "htree mounts while mounted");
    let mounts_stdout = String::from_utf8(mounts_output.stdout).expect("stdout utf-8");
    assert!(
        mounts_stdout.contains(&mountpoint.display().to_string()),
        "expected mounts output to include mountpoint.\nstdout:\n{mounts_stdout}"
    );
    assert!(
        mounts_stdout.contains(&nhash),
        "expected mounts output to include target/root.\nstdout:\n{mounts_stdout}"
    );

    let mounts_json_output = run_htree_command(&config_dir, &data_dir, ["mounts", "--json"]);
    assert_success(&mounts_json_output, "htree mounts --json while mounted");
    let mounts_json: Value =
        serde_json::from_slice(&mounts_json_output.stdout).expect("parse mounts json");
    let mounts = mounts_json.as_array().expect("mounts array");
    assert_eq!(mounts.len(), 1, "expected one active mount in json");
    let mount = mounts[0].as_object().expect("mount entry object");
    assert_eq!(
        mount.get("mountpoint").and_then(Value::as_str),
        Some(mountpoint.to_string_lossy().as_ref())
    );
    assert_eq!(
        mount.get("target").and_then(Value::as_str),
        Some(nhash.as_str())
    );
    assert_eq!(
        mount.get("mounted_cid").and_then(Value::as_str),
        Some(nhash.as_str())
    );
    assert_eq!(
        mount.get("visibility").and_then(Value::as_str),
        Some("public")
    );
    assert_eq!(
        mount.get("pid").and_then(Value::as_u64),
        Some(child.child_mut().id().into())
    );
    assert_eq!(mount.get("published_key"), Some(&Value::Null));
    assert_eq!(
        mount.get("allow_other").and_then(Value::as_bool),
        Some(cfg!(target_os = "macos"))
    );

    unmount(&mountpoint);

    let mount_exit = child.wait_with_output();
    assert_success(&mount_exit, "mounted htree process");

    let post_unmount = run_htree_command(&config_dir, &data_dir, ["mounts"]);
    assert_success(&post_unmount, "htree mounts after unmount");
    let post_unmount_stdout = String::from_utf8(post_unmount.stdout).expect("stdout utf-8");
    assert!(
        post_unmount_stdout.contains(EMPTY_MOUNTS_OUTPUT),
        "expected mounts output to be empty after unmount.\nstdout:\n{post_unmount_stdout}"
    );
}

fn run_htree_command<const N: usize>(
    config_dir: &Path,
    data_dir: &Path,
    args: [&str; N],
) -> Output {
    Command::new(htree_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env("HTREE_CONFIG_DIR", config_dir)
        .output()
        .expect("run htree command")
}

fn assert_success(output: &Output, description: &str) {
    assert!(
        output.status.success(),
        "{description} failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_mounted_file(
    child: &mut ChildGuard,
    config_dir: &Path,
    data_dir: &Path,
    mountpoint: &Path,
    path: PathBuf,
    expected: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.child_mut().try_wait().expect("poll child") {
            panic!("mount process exited early: {status}");
        }
        if !mountpoint_is_active(mountpoint) {
            if Instant::now() >= deadline {
                let registry_output = run_htree_command(config_dir, data_dir, ["mounts", "--json"]);
                let mountinfo = current_mountinfo();
                let root_snapshot = snapshot_dir(mountpoint);
                let mount_output = child.stop_after_unmount(mountpoint);
                panic!(
                    "timed out waiting for active mount {}\nmountinfo:\n{}\nroot snapshot:\n{}\nregistry stdout:\n{}\nregistry stderr:\n{}\nmount stdout:\n{}\nmount stderr:\n{}",
                    mountpoint.display(),
                    mountinfo,
                    root_snapshot,
                    String::from_utf8_lossy(&registry_output.stdout),
                    String::from_utf8_lossy(&registry_output.stderr),
                    String::from_utf8_lossy(&mount_output.stdout),
                    String::from_utf8_lossy(&mount_output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if contents == expected {
                return;
            }
        }
        if Instant::now() >= deadline {
            let registry_output = run_htree_command(config_dir, data_dir, ["mounts", "--json"]);
            let mountinfo = current_mountinfo();
            let root_snapshot = snapshot_dir(mountpoint);
            let parent_snapshot = snapshot_dir(path.parent().unwrap_or(mountpoint));
            let mount_output = child.stop_after_unmount(mountpoint);
            panic!(
                "timed out waiting for mounted file {}\nmountinfo:\n{}\nroot snapshot:\n{}\nparent snapshot:\n{}\nregistry stdout:\n{}\nregistry stderr:\n{}\nmount stdout:\n{}\nmount stderr:\n{}",
                path.display(),
                mountinfo,
                root_snapshot,
                parent_snapshot,
                String::from_utf8_lossy(&registry_output.stdout),
                String::from_utf8_lossy(&registry_output.stderr),
                String::from_utf8_lossy(&mount_output.stdout),
                String::from_utf8_lossy(&mount_output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn unmount(mountpoint: &Path) {
    if try_unmount(mountpoint) {
        return;
    }

    panic!("failed to unmount {}", mountpoint.display());
}

fn try_unmount(mountpoint: &Path) -> bool {
    #[cfg(target_os = "linux")]
    let commands: &[(&str, &[&str])] = &[
        ("fusermount3", &["-u"]),
        ("fusermount", &["-u"]),
        ("umount", &[]),
    ];
    #[cfg(not(target_os = "linux"))]
    let commands: &[(&str, &[&str])] = &[("umount", &[])];

    for (command, args) in commands {
        let status = Command::new(command).args(*args).arg(mountpoint).status();
        match status {
            Ok(status) if status.success() => return true,
            Ok(_) | Err(_) => continue,
        }
    }

    false
}

fn fuse_smoke_skip_reason() -> Option<String> {
    if std::env::var_os("HTREE_REQUIRE_FUSE_SMOKE").is_some() {
        return None;
    }

    #[cfg(not(target_os = "linux"))]
    {
        Some(
            "automatic FUSE smoke coverage only runs on Linux; set HTREE_REQUIRE_FUSE_SMOKE=1 to force a local run"
                .to_string(),
        )
    }

    #[cfg(target_os = "linux")]
    {
        if !Path::new("/dev/fuse").exists() {
            return Some("/dev/fuse is not available".to_string());
        }

        return None;
    }
}

fn snapshot_dir(path: &Path) -> String {
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut names = entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            if names.is_empty() {
                "(empty)".to_string()
            } else {
                names.join("\n")
            }
        }
        Err(error) => format!("read_dir failed: {error}"),
    }
}

#[cfg(target_os = "linux")]
fn mountpoint_is_active(mountpoint: &Path) -> bool {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mountinfo_lists_mount(&mountinfo, mountpoint)
}

#[cfg(not(target_os = "linux"))]
fn mountpoint_is_active(_mountpoint: &Path) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn current_mountinfo() -> String {
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_else(|error| format!("failed to read /proc/self/mountinfo: {error}"))
}

#[cfg(not(target_os = "linux"))]
fn current_mountinfo() -> String {
    "mountinfo unavailable on this platform".to_string()
}

fn mountinfo_lists_mount(mountinfo: &str, mountpoint: &Path) -> bool {
    let expected = mountpoint.to_string_lossy();
    mountinfo.lines().any(|line| {
        let Some(fields) = line.split(" - ").next() else {
            return false;
        };
        let mut parts = fields.split(' ');
        let _mount_id = parts.next();
        let _parent_id = parts.next();
        let _major_minor = parts.next();
        let _root = parts.next();
        let Some(mount_point) = parts.next() else {
            return false;
        };
        decode_mountinfo_path(mount_point) == expected
    })
}

fn decode_mountinfo_path(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let maybe_octal = &value[index + 1..index + 4];
            if maybe_octal
                .bytes()
                .all(|byte| (b'0'..=b'7').contains(&byte))
            {
                let octal = u8::from_str_radix(maybe_octal, 8).expect("valid octal escape");
                decoded.push(octal);
                index += 4;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).expect("mountinfo path is valid utf-8")
}

#[test]
fn mountinfo_parser_matches_escaped_mountpoint_paths() {
    let mountinfo = "\
35 26 0:31 / /tmp/hashtree\\040mount rw,nosuid,nodev - fuse.hashtree hashtree rw,user_id=1000,group_id=1000\n\
";

    assert!(mountinfo_lists_mount(
        mountinfo,
        Path::new("/tmp/hashtree mount")
    ));
}

#[test]
fn mountinfo_parser_ignores_other_mountpoints() {
    let mountinfo = "\
35 26 0:31 / /tmp/other rw,nosuid,nodev - fuse.hashtree hashtree rw,user_id=1000,group_id=1000\n\
";

    assert!(!mountinfo_lists_mount(
        mountinfo,
        Path::new("/tmp/hashtree mount")
    ));
}
