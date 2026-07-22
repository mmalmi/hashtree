//! E2E test: Git push/pull between two peers via FIPS peer transport
//!
//! Tests bidirectional git operations with multiple commits going back and forth.
//! Uses local TestRelay for FIPS discovery/signaling - no external network needed.

mod common;

use common::test_relay::TestRelay;
use common::{command_output_with_timeout, create_test_repo};
use git_remote_htree::nostr_client::KIND_HASHTREE_ROOT;
use nostr::{Keys, ToBech32};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

/// Test peer with htree daemon
struct TestPeer {
    _data_dir: TempDir,
    _home_dir: TempDir,
    process: Option<Child>,
    port: u16,
    npub: String,
    home_path: PathBuf,
}

impl TestPeer {
    fn new(
        port: u16,
        htree_bin: &str,
        keys: &Keys,
        follow_pubkeys: &[String],
        relay_url: &str,
    ) -> Self {
        let data_dir = TempDir::new().expect("Failed to create data dir");
        let home_dir = TempDir::new().expect("Failed to create home dir");
        let home_path = home_dir.path().to_path_buf();

        let config_dir = home_path.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");
        let fips_udp_port = port + 1000;

        let config_content = format!(
            r#"
[server]
bind_address = "127.0.0.1:{port}"
enable_auth = false
fips_udp_bind_addr = "127.0.0.1:{fips_udp_port}"
fips_udp_public = true
fips_udp_external_addr = "127.0.0.1:{fips_udp_port}"
public_writes = true

[nostr]
relays = ["{relay_url}"]

[blossom]
read_servers = ["http://127.0.0.1:{port}"]
write_servers = ["http://127.0.0.1:{port}"]

[sync]
enabled = false
"#,
            relay_url = relay_url,
            port = port,
            fips_udp_port = fips_udp_port,
        );
        std::fs::write(config_dir.join("config.toml"), &config_content)
            .expect("Failed to write config");

        let nsec = keys
            .secret_key()
            .to_bech32()
            .expect("Failed to encode nsec");
        let npub = keys
            .public_key()
            .to_bech32()
            .expect("Failed to encode npub");
        std::fs::write(config_dir.join("keys"), format!("{} self\n", nsec))
            .expect("Failed to write keys");

        if !follow_pubkeys.is_empty() {
            let contacts_json =
                serde_json::to_string(&follow_pubkeys).expect("Failed to serialize contacts");
            std::fs::write(data_dir.path().join("contacts.json"), &contacts_json)
                .expect("Failed to write contacts");
        }

        let process = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", port))
            .env("HOME", &home_path)
            .env(
                "RUST_LOG",
                std::env::var("HTREE_TEST_RUST_LOG").unwrap_or_else(|_| "warn".to_string()),
            )
            .stdout(if std::env::var("HTREE_TEST_STDIO").is_ok() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .stderr(if std::env::var("HTREE_TEST_STDIO").is_ok() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()
            .expect("Failed to start htree daemon");

        TestPeer {
            _data_dir: data_dir,
            _home_dir: home_dir,
            process: Some(process),
            port,
            npub,
            home_path,
        }
    }

    fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn git_command(&self, args: &[&str], cwd: &Path) -> Command {
        let bin_dir = find_bin_dir().expect("Binary dir not found");
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env("HOME", &self.home_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        if let Ok(log_filter) = std::env::var("HTREE_TEST_GIT_RUST_LOG") {
            cmd.env("RUST_LOG", log_filter);
        }
        cmd
    }

    fn git(&self, args: &[&str], cwd: &Path) -> std::process::Output {
        self.git_command(args, cwd)
            .output()
            .expect("Failed to run git")
    }

    fn git_with_timeout(
        &self,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<std::process::Output, String> {
        let mut command = self.git_command(args, cwd);
        command_output_with_timeout(&mut command, timeout)
            .map_err(|err| format!("git {}: {err}", args.join(" ")))
    }

    fn git_ok(&self, args: &[&str], cwd: &Path) {
        let out = self.git(args, cwd);
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_with_timeout_retry(
        &self,
        args: &[&str],
        cwd: &Path,
        attempts: usize,
        timeout: Duration,
    ) -> Result<std::process::Output, String> {
        let mut last_error = String::new();
        for attempt in 1..=attempts {
            match self.git_with_timeout(args, cwd, timeout) {
                Ok(out) if out.status.success() => return Ok(out),
                Ok(out) => {
                    last_error = format!(
                        "stdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                Err(err) => {
                    last_error = err;
                }
            }
            if attempt < attempts {
                std::thread::sleep(Duration::from_secs(2));
            }
        }

        Err(format!(
            "git {} failed after {} attempt(s): {}",
            args.join(" "),
            attempts,
            last_error
        ))
    }

    fn git_clone_ok_retry(&self, remote: &str, dest: &str, cwd: &Path) {
        let dest_path = cwd.join(dest);
        let mut last_stderr = String::new();
        for attempt in 1..=10 {
            if dest_path.exists() {
                std::fs::remove_dir_all(&dest_path).expect("remove failed clone destination");
            }
            match self.git_with_timeout(&["clone", remote, dest], cwd, Duration::from_secs(120)) {
                Ok(out) if out.status.success() => return,
                Ok(out) => {
                    last_stderr = String::from_utf8_lossy(&out.stderr).to_string();
                }
                Err(err) => {
                    last_stderr = err;
                }
            }
            if attempt < 10 {
                std::thread::sleep(Duration::from_secs(2));
            }
        }

        panic!("git clone {remote} {dest} failed after retries: {last_stderr}");
    }

    fn git_push_root(
        &self,
        args: &[&str],
        cwd: &Path,
        relay: &TestRelay,
        author: &str,
        repo: &str,
    ) -> String {
        let previous_ids = matching_root_event_ids(relay, author, repo);
        let mut last_error = String::new();
        for attempt in 1..=5 {
            match self.git_with_timeout(args, cwd, Duration::from_secs(120)) {
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if out.status.success() || stderr.contains("-> master") {
                        if let Some(hash) =
                            wait_for_new_root_event(relay, author, repo, &previous_ids)
                        {
                            return hash;
                        }
                        last_error = format!(
                            "push updated the ref but no new {author}/{repo} root reached the relay"
                        );
                    } else {
                        last_error = format!(
                            "stdout:\n{}\nstderr:\n{}",
                            String::from_utf8_lossy(&out.stdout),
                            stderr
                        );
                    }
                }
                Err(error) => last_error = error,
            }
            if attempt < 5 {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        panic!(
            "git {} did not publish a new root after retries: {last_error}",
            args.join(" ")
        );
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        if let Some(ref mut process) = self.process {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

fn find_htree_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_htree") {
        return Some(PathBuf::from(path));
    }

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();
    let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root.join("target"),
    };
    let debug_bin = target_dir.join("debug/htree");
    let release_bin = target_dir.join("release/htree");
    if debug_bin.exists() {
        Some(debug_bin)
    } else if release_bin.exists() {
        Some(release_bin)
    } else {
        None
    }
}

fn find_bin_dir() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();
    let debug_dir = workspace_root.join("target/debug");
    let release_dir = workspace_root.join("target/release");
    if debug_dir.join("git-remote-htree").exists() {
        Some(debug_dir)
    } else if release_dir.join("git-remote-htree").exists() {
        Some(release_dir)
    } else {
        None
    }
}

fn wait_for_server(url: &str) -> bool {
    for _ in 0..30 {
        if let Ok(resp) = reqwest::blocking::get(format!("{}/health", url)) {
            if resp.status().is_success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn get_daemon_status(peer_url: &str) -> serde_json::Value {
    let url = format!("{}/api/status", peer_url);
    reqwest::blocking::get(&url)
        .expect("Failed to get status")
        .json()
        .expect("Failed to parse status JSON")
}

fn matching_root_events(relay: &TestRelay, pubkey: &str, repo: &str) -> Vec<serde_json::Value> {
    relay
        .stored_events()
        .into_iter()
        .filter(|event| {
            event.get("kind").and_then(|value| value.as_u64()) == Some(KIND_HASHTREE_ROOT as u64)
                && event.get("pubkey").and_then(|value| value.as_str()) == Some(pubkey)
                && event
                    .get("tags")
                    .and_then(|value| value.as_array())
                    .is_some_and(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array().is_some_and(|parts| {
                                parts.len() >= 2
                                    && parts[0].as_str() == Some("d")
                                    && parts[1].as_str() == Some(repo)
                            })
                        })
                    })
        })
        .collect()
}

fn matching_root_event_ids(relay: &TestRelay, pubkey: &str, repo: &str) -> HashSet<String> {
    matching_root_events(relay, pubkey, repo)
        .into_iter()
        .filter_map(|event| {
            event
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn wait_for_new_root_event(
    relay: &TestRelay,
    pubkey: &str,
    repo: &str,
    previous_ids: &HashSet<String>,
) -> Option<String> {
    for _ in 0..100 {
        let newest = matching_root_events(relay, pubkey, repo)
            .into_iter()
            .filter(|event| {
                event
                    .get("id")
                    .and_then(|id| id.as_str())
                    .is_some_and(|id| !previous_ids.contains(id))
            })
            .max_by(|left, right| {
                left.get("created_at")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
                    .cmp(
                        &right
                            .get("created_at")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0),
                    )
                    .then_with(|| {
                        left.get("id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .cmp(
                                right
                                    .get("id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or(""),
                            )
                    })
            });
        if let Some(hash) = newest
            .as_ref()
            .and_then(|event| event.get("content"))
            .and_then(|content| content.as_str())
        {
            return Some(hash.to_owned());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn wait_for_daemon_root(peer: &TestPeer, owner_npub: &str, repo: &str, hash: &str) -> bool {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build daemon refresh client");
    let url = format!(
        "{}/api/nostr/resolve/{}/{}?refresh=1",
        peer.api_url(),
        owner_npub,
        repo
    );
    for _ in 0..10 {
        let matches = client
            .get(&url)
            .send()
            .and_then(|response| response.json::<serde_json::Value>())
            .ok()
            .and_then(|json| {
                json.get("hash")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .is_some_and(|resolved| resolved == hash);
        if matches {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn daemon_refresh(peer: &TestPeer, owner_npub: &str, repo: &str) -> String {
    reqwest::blocking::get(format!(
        "{}/api/nostr/resolve/{}/{}?refresh=1",
        peer.api_url(),
        owner_npub,
        repo
    ))
    .and_then(|response| response.text())
    .unwrap_or_else(|err| format!("daemon refresh failed: {err}"))
}

fn root_diagnostics(
    relay: &TestRelay,
    peer: &TestPeer,
    owner_hex: &str,
    owner_npub: &str,
) -> String {
    let events = matching_root_events(relay, owner_hex, "shared-repo");
    let events = serde_json::to_string_pretty(&events).unwrap_or_else(|_| format!("{events:?}"));
    let daemon = daemon_refresh(peer, owner_npub, "shared-repo");
    format!("relay events:\n{events}\ndaemon refresh:\n{daemon}")
}

fn wait_for_fips_peer(peer_url: &str, target_npub: &str) -> bool {
    for attempt in 1..=30 {
        if let Ok(resp) = reqwest::blocking::get(format!("{}/api/status", peer_url)) {
            if let Ok(text) = resp.text() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let has_peer = json
                        .get("fips")
                        .and_then(|fips| fips.get("peers"))
                        .and_then(|peers| peers.as_array())
                        .map(|peers| peers.iter().any(|peer| peer.as_str() == Some(target_npub)))
                        .unwrap_or(false);
                    if has_peer {
                        println!("  FIPS peer discovered after {}s", attempt * 2);
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

#[test]
fn test_p2p_git_roundtrip() {
    // Check prerequisites
    let htree_bin = match find_htree_binary() {
        Some(b) => b,
        None => {
            println!("SKIP: htree binary not found");
            return;
        }
    };
    if find_bin_dir().is_none() {
        println!("SKIP: git-remote-htree binary not found");
        return;
    }

    println!("=== P2P Git Roundtrip Test ===\n");

    // Start local relay
    let relay = TestRelay::new();
    let relay_url = relay.url();
    println!("Relay: {}", relay_url);

    // Generate keys
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let pubkey_a = keys_a.public_key().to_hex();
    let pubkey_b = keys_b.public_key().to_hex();

    // Start peers
    println!("Starting peers...");
    let peer_a = TestPeer::new(
        19091,
        htree_bin.to_str().unwrap(),
        &keys_a,
        std::slice::from_ref(&pubkey_b),
        &relay_url,
    );
    let peer_b = TestPeer::new(
        19092,
        htree_bin.to_str().unwrap(),
        &keys_b,
        std::slice::from_ref(&pubkey_a),
        &relay_url,
    );
    assert!(wait_for_server(&peer_a.api_url()), "Peer A failed to start");
    assert!(wait_for_server(&peer_b.api_url()), "Peer B failed to start");
    println!("Peers ready\n");

    // === Peer A: Create and push initial repo ===
    println!("1. Peer A: Creating and pushing repo...");
    let repo_a = create_test_repo();
    std::fs::write(repo_a.path().join("count.txt"), "1").unwrap();
    peer_a.git_ok(&["add", "count.txt"], repo_a.path());
    peer_a.git_ok(&["commit", "-m", "Add count"], repo_a.path());
    peer_a.git_ok(
        &["remote", "add", "origin", "htree://self/shared-repo"],
        repo_a.path(),
    );
    let first_a_root = peer_a.git_push_root(
        &["push", "-u", "origin", "master"],
        repo_a.path(),
        &relay,
        &pubkey_a,
        "shared-repo",
    );
    println!("   Pushed (count=1)\n");

    // Wait for FIPS peer discovery (both directions)
    println!("2. Waiting for FIPS peer discovery...");
    assert!(
        wait_for_fips_peer(&peer_a.api_url(), &peer_b.npub),
        "FIPS A->B peer discovery failed"
    );
    assert!(
        wait_for_fips_peer(&peer_b.api_url(), &peer_a.npub),
        "FIPS B->A peer discovery failed"
    );

    // === Verify status endpoint shows connection ===
    println!("   Verifying /api/status...");
    let status_a = get_daemon_status(&peer_a.api_url());
    let status_b = get_daemon_status(&peer_b.api_url());

    let fips_a = status_a.get("fips").expect("status should have fips");
    let fips_b = status_b.get("fips").expect("status should have fips");

    assert!(
        fips_a
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        "FIPS should be enabled"
    );
    assert!(
        fips_b
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        "FIPS should be enabled"
    );

    let peers_a = fips_a
        .get("peers")
        .and_then(|peers| peers.as_array())
        .cloned()
        .unwrap_or_default();
    let peers_b = fips_b
        .get("peers")
        .and_then(|peers| peers.as_array())
        .cloned()
        .unwrap_or_default();

    assert!(
        peers_a
            .iter()
            .any(|peer| peer.as_str() == Some(peer_b.npub.as_str())),
        "Peer A should discover Peer B over FIPS, got {:?}",
        peers_a
    );
    assert!(
        peers_b
            .iter()
            .any(|peer| peer.as_str() == Some(peer_a.npub.as_str())),
        "Peer B should discover Peer A over FIPS, got {:?}",
        peers_b
    );
    println!(
        "   Status verified: A has {} FIPS peers, B has {} FIPS peers",
        peers_a.len(),
        peers_b.len()
    );

    // === Peer B: Clone the repo ===
    println!("\n3. Peer B: Cloning repo...");
    assert!(
        wait_for_daemon_root(&peer_b, &peer_a.npub, "shared-repo", &first_a_root),
        "Peer B must resolve Peer A's initial root before cloning"
    );
    let clone_dir_b = TempDir::new().unwrap();
    let repo_b_path = clone_dir_b.path().join("repo");
    peer_b.git_clone_ok_retry(
        &format!("htree://{}/shared-repo", peer_a.npub),
        "repo",
        clone_dir_b.path(),
    );

    // Verify clone content
    let count = std::fs::read_to_string(repo_b_path.join("count.txt")).unwrap();
    assert_eq!(count.trim(), "1", "Initial clone should have count=1");
    assert!(
        repo_b_path.join("README.md").exists(),
        "README.md should exist"
    );
    println!("   Cloned and verified (count=1)\n");

    // Configure git for cloned repo
    peer_b.git_ok(&["config", "user.email", "peerb@test.local"], &repo_b_path);
    peer_b.git_ok(&["config", "user.name", "Peer B"], &repo_b_path);

    // === Peer B: Make changes and push ===
    println!("4. Peer B: Updating and pushing...");
    std::fs::write(repo_b_path.join("count.txt"), "2").unwrap();
    std::fs::write(repo_b_path.join("from_b.txt"), "Added by Peer B").unwrap();
    peer_b.git_ok(&["add", "."], &repo_b_path);
    peer_b.git_ok(&["commit", "-m", "Peer B: count=2"], &repo_b_path);
    peer_b.git_ok(
        &["remote", "set-url", "origin", "htree://self/shared-repo"],
        &repo_b_path,
    );
    let b_root = peer_b.git_push_root(
        &["push", "-u", "origin", "master"],
        &repo_b_path,
        &relay,
        &pubkey_b,
        "shared-repo",
    );
    assert!(
        wait_for_daemon_root(&peer_a, &peer_b.npub, "shared-repo", &b_root),
        "Peer A must resolve Peer B's updated root before pulling"
    );
    println!("   Pushed (count=2)\n");

    // === Peer A: Pull changes ===
    println!("5. Peer A: Pulling changes...");
    // Need to set remote to peer B's npub to pull their version
    peer_a.git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            &format!("htree://{}/shared-repo", peer_b.npub),
        ],
        repo_a.path(),
    );
    peer_a
        .git_with_timeout_retry(
            &["pull", "--rebase"],
            repo_a.path(),
            3,
            Duration::from_secs(90),
        )
        .unwrap_or_else(|err| panic!("Peer A pull failed: {err}"));

    let count = std::fs::read_to_string(repo_a.path().join("count.txt")).unwrap();
    assert_eq!(count.trim(), "2", "After pull, count should be 2");
    assert!(
        repo_a.path().join("from_b.txt").exists(),
        "from_b.txt should exist after pull"
    );
    println!("   Pulled and verified (count=2, from_b.txt exists)\n");

    // === Peer A: Make more changes and push ===
    println!("6. Peer A: Updating and pushing...");
    std::fs::write(repo_a.path().join("count.txt"), "3").unwrap();
    std::fs::write(repo_a.path().join("from_a.txt"), "Added by Peer A").unwrap();
    peer_a.git_ok(&["add", "."], repo_a.path());
    peer_a.git_ok(&["commit", "-m", "Peer A: count=3"], repo_a.path());
    peer_a.git_ok(
        &["remote", "set-url", "origin", "htree://self/shared-repo"],
        repo_a.path(),
    );
    let final_a_root =
        peer_a.git_push_root(&["push"], repo_a.path(), &relay, &pubkey_a, "shared-repo");
    assert!(
        wait_for_daemon_root(&peer_b, &peer_a.npub, "shared-repo", &final_a_root),
        "Peer B must resolve Peer A's final root before pulling"
    );
    println!("   Pushed (count=3)\n");

    // === Peer B: Pull final changes ===
    println!("7. Peer B: Pulling final changes...");
    peer_b.git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            &format!("htree://{}/shared-repo", peer_a.npub),
        ],
        &repo_b_path,
    );
    let final_pull = match peer_b.git_with_timeout_retry(
        &["pull", "--rebase"],
        &repo_b_path,
        3,
        Duration::from_secs(90),
    ) {
        Ok(out) => out,
        Err(err) => {
            panic!(
                "final pull failed\n{}\n{}",
                err,
                root_diagnostics(&relay, &peer_b, &pubkey_a, &peer_a.npub)
            );
        }
    };
    let count = std::fs::read_to_string(repo_b_path.join("count.txt")).unwrap();
    if count.trim() != "3" {
        let log = peer_b.git(
            &["log", "--oneline", "--decorate", "--graph", "--all", "-8"],
            &repo_b_path,
        );
        let status = peer_b.git(&["status", "-sb"], &repo_b_path);
        let branches = peer_b.git(&["branch", "-vv"], &repo_b_path);
        eprintln!(
            "final pull stdout:\n{}\nfinal pull stderr:\n{}\nlog:\n{}\nstatus:\n{}\nbranches:\n{}\n{}",
            String::from_utf8_lossy(&final_pull.stdout),
            String::from_utf8_lossy(&final_pull.stderr),
            String::from_utf8_lossy(&log.stdout),
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&branches.stdout),
            root_diagnostics(&relay, &peer_b, &pubkey_a, &peer_a.npub)
        );
    }
    assert_eq!(count.trim(), "3", "Final count should be 3");
    assert!(
        repo_b_path.join("from_a.txt").exists(),
        "from_a.txt should exist"
    );
    assert!(
        repo_b_path.join("from_b.txt").exists(),
        "from_b.txt should still exist"
    );
    println!("   Pulled and verified (count=3, both files exist)\n");

    println!("=== SUCCESS: FIPS Git roundtrip complete! ===");
}
