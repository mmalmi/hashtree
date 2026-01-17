//! Basic git push and clone tests
//!
//! Tests the fundamental git remote helper workflow:
//! - Push to htree://
//! - Clone from htree://
//! - Verify files match

mod common;

use common::{test_relay::TestRelay, TestServer, TestEnv, create_test_repo, skip_if_no_binary};
use std::process::Command;
use tempfile::TempDir;

/// Test git push and clone with local servers (no network needed)
#[test]
fn test_git_push_and_clone_local() {
    if skip_if_no_binary() {
        return;
    }

    // Start local nostr relay
    let relay = TestRelay::new(19300);
    println!("Started local nostr relay at: {}", relay.url());

    // Start local blossom server
    let server = match TestServer::new(19301) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    println!("Started local blossom server at: {}", server.base_url());

    println!("\n=== Git Push/Clone Roundtrip Test (Local Servers) ===\n");

    // Create test environment pointing to local servers
    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    println!("Test environment at: {:?}\n", test_env.home_dir);

    // Create test repo
    println!("Creating test repository...");
    let repo = create_test_repo();
    println!("Test repo at: {:?}\n", repo.path());

    // Add htree remote
    let remote_url = "htree://self/test-repo-local";
    println!("Adding remote: {}", remote_url);

    let env_vars: Vec<_> = test_env.env();

    let add_remote = Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    if !add_remote.status.success() {
        panic!(
            "git remote add failed: {}",
            String::from_utf8_lossy(&add_remote.stderr)
        );
    }

    // Push to htree
    println!("\nPushing to htree...");
    let push_start = std::time::Instant::now();

    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git push");

    let push_duration = push_start.elapsed();
    println!("Push stderr: {}", String::from_utf8_lossy(&push.stderr));
    println!("Push took: {:?}", push_duration);

    let stderr = String::from_utf8_lossy(&push.stderr);
    let push_worked = stderr.contains("-> master") || stderr.contains("-> main");

    if !push.status.success() && !push_worked {
        panic!("git push failed: {}", stderr);
    }
    println!("Push successful!\n");

    // Clone using the npub
    let npub = &test_env.npub;
    let clone_url = format!("htree://{}/test-repo-local", npub);
    let clone_dir = TempDir::new().expect("Failed to create clone dir");
    let clone_path = clone_dir.path().join("cloned-repo");

    println!("Cloning from {} to {:?}...", clone_url, clone_path);
    let clone_start = std::time::Instant::now();

    let clone = Command::new("git")
        .args(["clone", &clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git clone");

    let clone_duration = clone_start.elapsed();
    println!("Clone stderr: {}", String::from_utf8_lossy(&clone.stderr));
    println!("Clone took: {:?}", clone_duration);

    if !clone.status.success() {
        panic!("git clone failed: {}", String::from_utf8_lossy(&clone.stderr));
    }
    println!("Clone successful!\n");

    // Verify files match
    println!("Verifying files...");

    let original_readme = std::fs::read_to_string(repo.path().join("README.md")).unwrap();
    let cloned_readme = std::fs::read_to_string(clone_path.join("README.md")).unwrap();
    assert_eq!(original_readme, cloned_readme, "README.md should match");

    let original_hello = std::fs::read_to_string(repo.path().join("hello.txt")).unwrap();
    let cloned_hello = std::fs::read_to_string(clone_path.join("hello.txt")).unwrap();
    assert_eq!(original_hello, cloned_hello, "hello.txt should match");

    let original_main = std::fs::read_to_string(repo.path().join("src/main.rs")).unwrap();
    let cloned_main = std::fs::read_to_string(clone_path.join("src/main.rs")).unwrap();
    assert_eq!(original_main, cloned_main, "src/main.rs should match");

    println!("\n=== SUCCESS: Local git roundtrip test passed! ===");
    println!("Push time: {:?}", push_duration);
    println!("Clone time: {:?}", clone_duration);
}

#[test]
fn test_git_remote_htree_binary_exists() {
    if skip_if_no_binary() {
        return;
    }

    let bin_dir = common::find_git_remote_htree_dir().unwrap();
    let binary = bin_dir.join("git-remote-htree");
    assert!(binary.exists(), "git-remote-htree binary should exist");
}
