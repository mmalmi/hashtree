//! Production server tests
//!
//! These tests require network access and are ignored by default.
//! Run with: cargo test --test production -- --ignored --nocapture

mod common;

use common::{TestEnv, create_test_repo, find_git_remote_htree_dir};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Test with production servers (requires network)
#[test]
#[ignore = "requires network - run with: cargo test --test production test_git_push_and_clone_production -- --ignored --nocapture"]
fn test_git_push_and_clone_production() {
    if find_git_remote_htree_dir().is_none() {
        panic!("git-remote-htree binary not found");
    }

    println!("=== Git Push/Clone Roundtrip Test (Production Servers) ===\n");

    let test_env = TestEnv::new(None, None);
    let repo = create_test_repo();
    let env_vars: Vec<_> = test_env.env();

    Command::new("git")
        .args(["remote", "add", "htree", "htree://self/test-repo"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr = String::from_utf8_lossy(&push.stderr);
    println!("Push stderr: {}", stderr);

    if !push.status.success() && !stderr.contains("-> master") {
        panic!("git push failed: {}", stderr);
    }

    let npub = &test_env.npub;
    let clone_url = format!("htree://{}/test-repo", npub);
    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("cloned");

    let clone = Command::new("git")
        .args(["clone", &clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to clone");

    if !clone.status.success() {
        panic!("git clone failed: {}", String::from_utf8_lossy(&clone.stderr));
    }

    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        std::fs::read_to_string(clone_path.join("README.md")).unwrap()
    );

    println!("\n=== SUCCESS ===");
}

/// Test diff-based push with production servers
#[test]
#[ignore = "requires network - run with: cargo test --test production test_diff_based_push_production -- --ignored --nocapture"]
fn test_diff_based_push_production() {
    if find_git_remote_htree_dir().is_none() {
        panic!("git-remote-htree binary not found");
    }

    println!("=== Diff-Based Push Test (Production Servers) ===\n");

    let test_env = TestEnv::new(None, None);
    let repo = create_test_repo();
    let env_vars: Vec<_> = test_env.env();

    let remote_url = "htree://self/diff-test-prod";
    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    // First push
    println!("=== First push (full upload) ===");
    let push1 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr1 = String::from_utf8_lossy(&push1.stderr);
    println!("{}", stderr1);
    if !push1.status.success() && !stderr1.contains("-> master") {
        panic!("First push failed: {}", stderr1);
    }

    // Make small change
    println!("\n=== Making small change ===");
    std::fs::write(repo.path().join("new-file.txt"), "Small change\n").unwrap();
    Command::new("git").args(["add", "."]).current_dir(repo.path()).output().unwrap();
    Command::new("git").args(["commit", "-m", "Add file"]).current_dir(repo.path())
        .stdout(Stdio::null()).output().unwrap();

    // Second push - should use diff
    println!("\n=== Second push (should use diff) ===");
    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    println!("{}", stderr2);
    if !push2.status.success() && !stderr2.contains("-> master") {
        panic!("Second push failed: {}", stderr2);
    }

    let used_diff = stderr2.contains("unchanged") || stderr2.contains("Computing diff");
    println!("\nDiff optimization used: {}", used_diff);
    assert!(used_diff, "Second push should use diff optimization");

    println!("\n=== SUCCESS ===");
}
