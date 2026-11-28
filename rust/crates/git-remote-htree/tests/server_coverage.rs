//! Server coverage tests.
//!
//! An optional writer can lack retention permission or go offline between pushes.

mod common;

use common::{create_test_repo, skip_if_no_binary, test_relay::TestRelay, TestEnv, TestServer};
use std::process::{Command, Stdio};

/// The retained writer remains sufficient for a fresh reader after an optional writer goes away.
#[test]
fn test_optional_writer_is_not_required_for_cold_clone() {
    if skip_if_no_binary() {
        return;
    }

    // Start local relay
    let relay = TestRelay::new();
    println!("Started local nostr relay at: {}", relay.url());

    // Start TWO blossom servers
    let server_a = match TestServer::new() {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    println!("Started blossom server A at: {}", server_a.base_url());

    let server_b = match TestServer::new() {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    println!("Started blossom server B at: {}", server_b.base_url());

    println!("\n=== Server Coverage Test (Optional New Server) ===\n");

    // Create test environment with ONLY server A initially
    let test_env = server_a.writer_env(&relay.url());
    println!("Test environment at: {:?}\n", test_env.home_dir);

    let env_vars: Vec<_> = test_env.env();

    // Create and push initial repo to server A only
    let repo = create_test_repo();
    println!("Test repo at: {:?}\n", repo.path());

    let remote_url = "htree://self/coverage-test-repo";
    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    // First push - only to server A
    println!("=== First push (to server A only) ===");
    let push1 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr1 = String::from_utf8_lossy(&push1.stderr);
    println!("First push stderr:\n{}", stderr1);

    if !push1.status.success() {
        panic!("First push failed: {}", stderr1);
    }

    // Now update config to include BOTH servers
    println!("\n=== Adding server B to config ===");
    test_env.update_blossom_servers(&[&server_b.base_url(), &server_a.base_url()], &relay.url());

    // Make a small change
    std::fs::write(
        repo.path().join("new-file.txt"),
        "Testing server coverage\n",
    )
    .expect("Failed to write file");

    Command::new("git")
        .args(["add", "new-file.txt"])
        .current_dir(repo.path())
        .output()
        .expect("Failed to git add");

    Command::new("git")
        .args(["commit", "-m", "Add new file for coverage test"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .output()
        .expect("Failed to commit");

    // Server B accepts cache writes from this author. Server A can retain them.
    println!("\n=== Second push with optional cache writer ===");
    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    println!("Second push stderr:\n{}", stderr2);

    if !push2.status.success() {
        panic!("Second push failed: {}", stderr2);
    }

    drop(server_b);
    // Only the retained writer is reachable for the next publish and the cold clone.
    std::fs::write(
        repo.path().join("another-file.txt"),
        "Testing repeated server coverage\n",
    )
    .expect("Failed to write follow-up file");

    Command::new("git")
        .args(["add", "another-file.txt"])
        .current_dir(repo.path())
        .output()
        .expect("Failed to git add follow-up");

    Command::new("git")
        .args(["commit", "-m", "Add follow-up file for coverage test"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .output()
        .expect("Failed to commit follow-up");

    println!("\n=== Third push with the optional writer offline ===");
    let push3 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push follow-up");

    let stderr3 = String::from_utf8_lossy(&push3.stderr);
    println!("Third push stderr:\n{}", stderr3);

    if !push3.status.success() {
        panic!("Third push failed: {}", stderr3);
    }

    let reader = TestEnv::new(Some(&server_a.base_url()), Some(&relay.url()));
    let target = tempfile::TempDir::new().unwrap();
    let clone_url = format!("htree://{}/coverage-test-repo", test_env.npub);
    let clone = Command::new("git")
        .args(["clone", "--branch", "master", &clone_url, "."])
        .current_dir(target.path())
        .envs(reader.env())
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(target.path().join("another-file.txt")).unwrap(),
        "Testing repeated server coverage\n"
    );
    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(target.path())
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
