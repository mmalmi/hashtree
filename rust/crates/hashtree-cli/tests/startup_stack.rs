#![cfg(any(unix, windows))]

use std::fs::File;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn starts_on_a_small_main_stack() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "[updater]\nauto_check = false\n[server]\nenable_fips_lan_discovery = false\n",
    )
    .unwrap();
    let log_path = temp.path().join("startup.log");
    let log = File::create(&log_path).unwrap();
    #[cfg(unix)]
    let mut command = {
        // Set the fixture process's main-stack limit before exec. Doing this in
        // pre_exec from a test worker thread is unsupported on macOS.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "set -e; ulimit -c 0; ulimit -s 1024; exec \"$@\"",
            "htree-startup-test",
            env!("CARGO_BIN_EXE_htree"),
        ]);
        command
    };
    #[cfg(windows)]
    let mut command = Command::new(env!("CARGO_BIN_EXE_htree"));
    command
        .args(["--data-dir"])
        .arg(&data)
        .args(["start", "--addr", "127.0.0.1:0", "--relays", ""])
        .env("HOME", temp.path())
        .env("HTREE_CONFIG_DIR", &config)
        .env("HTREE_DATA_DIR", &data)
        .env("HTREE_ALLOW_ROOT_DAEMON", "1")
        .env("TOKIO_WORKER_THREADS", "2")
        .env("NOSTR_RELAYS", "")
        .env("NOSTR_PREFER_LOCAL", "0")
        .env(
            "RUST_LOG",
            "warn,fips_core::transport::webrtc=info,fips_core::node::lifecycle::runtime=info",
        )
        .env_remove("NOSTR_SECRET_KEY")
        .env_remove("NOSTR_PRIVATE_KEY")
        .env_remove("NOSTR_KEY")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!(
                "bounded empty-relay startup did not finish:\n{}",
                std::fs::read_to_string(&log_path).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(log_path).unwrap();
    // Empty relays intentionally stop the fixture after transport startup.
    // A marker followed by a crash must never count as successful startup.
    assert_eq!(status.code(), Some(1), "startup exited {status}:\n{output}");
    assert!(
        output.contains("Failed to start Nostr relay event provider")
            && output.contains("add relay : relative URL without a base"),
        "unexpected startup termination {status}:\n{output}"
    );
    #[cfg(feature = "fips-webrtc")]
    assert!(
        output.contains("WebRTC transport started"),
        "startup exited {status}:\n{output}"
    );
    #[cfg(feature = "fips-webrtc")]
    assert!(!output.contains("built without the webrtc feature"));
    assert!(
        output.contains("Node started:"),
        "startup exited {status}:\n{output}"
    );
}
