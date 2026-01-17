//! Non-fast-forward push tests
//!
//! Tests that divergent pushes are rejected unless --force is used.

mod common;

use common::{test_relay::TestRelay, TestServer, TestEnv, create_test_repo, skip_if_no_binary};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Test that non-fast-forward pushes are rejected unless --force is used
#[test]
fn test_non_fast_forward_rejected() {
    if skip_if_no_binary() {
        return;
    }

    // Start local servers
    let relay = TestRelay::new(19220);
    let server = match TestServer::new(19221) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    println!("\n=== Non-Fast-Forward Push Test ===\n");

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    // Create repo A and push initial commit
    let repo_a = create_test_repo();
    println!("Repo A at: {:?}", repo_a.path());

    Command::new("git")
        .args(["remote", "add", "htree", "htree://self/nff-test"])
        .current_dir(repo_a.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    // Initial push from A
    println!("=== Initial push from repo A ===");
    let push1 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo_a.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr1 = String::from_utf8_lossy(&push1.stderr);
    println!("{}", stderr1);
    assert!(
        push1.status.success() || stderr1.contains("-> master"),
        "Initial push should succeed"
    );

    // Clone to repo B
    let npub = &test_env.npub;
    let clone_url = format!("htree://{}/nff-test", npub);
    let repo_b_dir = TempDir::new().expect("Failed to create temp dir");
    let repo_b = repo_b_dir.path().join("repo-b");

    println!("\n=== Clone to repo B ===");
    let clone = Command::new("git")
        .args(["clone", &clone_url, repo_b.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to clone");

    assert!(clone.status.success(), "Clone should succeed");

    // Configure repo B
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&repo_b)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&repo_b)
        .output()
        .unwrap();
    Command::new("git")
        .args(["remote", "add", "htree", "htree://self/nff-test"])
        .current_dir(&repo_b)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .unwrap();

    // Make a commit in repo A and push
    println!("\n=== Make commit in repo A and push ===");
    std::fs::write(repo_a.path().join("from-a.txt"), "Commit from A\n").unwrap();
    Command::new("git")
        .args(["add", "from-a.txt"])
        .current_dir(repo_a.path())
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "Commit from A"])
        .current_dir(repo_a.path())
        .stdout(Stdio::null())
        .output()
        .unwrap();

    let push_a = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo_a.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push from A");

    let stderr_a = String::from_utf8_lossy(&push_a.stderr);
    println!("{}", stderr_a);
    assert!(
        push_a.status.success() || stderr_a.contains("-> master"),
        "Push from A should succeed"
    );

    // Make a different commit in repo B (divergent history)
    println!("\n=== Make divergent commit in repo B ===");
    std::fs::write(repo_b.join("from-b.txt"), "Commit from B\n").unwrap();
    Command::new("git")
        .args(["add", "from-b.txt"])
        .current_dir(&repo_b)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "Commit from B"])
        .current_dir(&repo_b)
        .stdout(Stdio::null())
        .output()
        .unwrap();

    // Try to push from B - should fail (non-fast-forward)
    println!("\n=== Try push from repo B (should fail) ===");
    let push_b = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(&repo_b)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run push from B");

    let stderr_b = String::from_utf8_lossy(&push_b.stderr);
    println!("{}", stderr_b);

    // Should contain non-fast-forward error
    assert!(
        stderr_b.contains("non-fast-forward") || stderr_b.contains("Rejected"),
        "Push from B should be rejected as non-fast-forward. Got: {}",
        stderr_b
    );

    // Force push from B - should succeed
    println!("\n=== Force push from repo B (should succeed) ===");
    let force_push_b = Command::new("git")
        .args(["push", "--force", "htree", "master"])
        .current_dir(&repo_b)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run force push from B");

    let stderr_force = String::from_utf8_lossy(&force_push_b.stderr);
    println!("{}", stderr_force);

    assert!(
        force_push_b.status.success() || stderr_force.contains("-> master"),
        "Force push from B should succeed. Got: {}",
        stderr_force
    );

    println!("\n=== SUCCESS: Non-fast-forward test passed! ===");
}
