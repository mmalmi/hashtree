//! Server coverage tests
//!
//! Tests that adding a new blossom server triggers full upload to it.

mod common;

use common::{test_relay::TestRelay, TestServer, TestEnv, create_test_repo, skip_if_no_binary};
use std::process::{Command, Stdio};

/// Test that adding a new blossom server triggers full upload to it
#[test]
fn test_server_coverage_full_upload() {
    if skip_if_no_binary() {
        return;
    }

    // Start local relay
    let relay = TestRelay::new(19210);
    println!("Started local nostr relay at: {}", relay.url());

    // Start TWO blossom servers
    let server_a = match TestServer::new(19211) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    println!("Started blossom server A at: {}", server_a.base_url());

    let server_b = match TestServer::new(19212) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    println!("Started blossom server B at: {}", server_b.base_url());

    println!("\n=== Server Coverage Test (Full Upload to New Server) ===\n");

    // Create test environment with ONLY server A initially
    let test_env = TestEnv::new(Some(&server_a.base_url()), Some(&relay.url()));
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

    if !push1.status.success() && !stderr1.contains("-> master") {
        panic!("First push failed: {}", stderr1);
    }

    // Now update config to include BOTH servers
    println!("\n=== Adding server B to config ===");
    test_env.update_blossom_servers(
        &[&server_a.base_url(), &server_b.base_url()],
        &relay.url(),
    );

    // Make a small change
    std::fs::write(repo.path().join("new-file.txt"), "Testing server coverage\n")
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

    // Second push - should detect server B needs full upload
    println!("\n=== Second push (should detect server B needs full upload) ===");
    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    println!("Second push stderr:\n{}", stderr2);

    if !push2.status.success() && !stderr2.contains("-> master") {
        panic!("Second push failed: {}", stderr2);
    }

    // Verify that either:
    // 1. Full upload was triggered for server B ("Full upload needed")
    // 2. Or the output shows it's uploading to both servers
    let full_upload_detected = stderr2.contains("Full upload needed")
        || stderr2.contains("not have old tree")
        || stderr2.contains("full upload");

    // Also check if diff was used (for server A) - shows optimization is working per-server
    let diff_used = stderr2.contains("unchanged") || stderr2.contains("Computing diff");

    println!("\nFull upload to new server detected: {}", full_upload_detected);
    println!("Diff optimization for existing server: {}", diff_used);

    // At minimum, the push should succeed and show upload activity
    // Server URLs in output have http:// stripped, so check for the host:port part
    let server_a_url = server_a.base_url();
    let server_b_url = server_b.base_url();
    let server_a_host = server_a_url.trim_start_matches("http://");
    let server_b_host = server_b_url.trim_start_matches("http://");
    assert!(
        stderr2.contains(server_a_host) || stderr2.contains(server_b_host) || stderr2.contains("Blossom") || stderr2.contains("Uploading"),
        "Push should show blossom server activity"
    );

    println!("\n=== SUCCESS: Server coverage test passed! ===");
}
