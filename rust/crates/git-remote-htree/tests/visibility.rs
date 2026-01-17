//! Visibility tests
//!
//! Tests that repos with visibility restrictions actually enforce them:
//! - Link-visible (#k=<secret>): Can only be read with the correct secret in URL
//! - Private (#private): Can only be read by the author (NOT YET IMPLEMENTED)

mod common;

use common::{test_relay::TestRelay, TestServer, TestEnv, create_test_repo, skip_if_no_binary};
use std::process::Command;
use tempfile::TempDir;

/// Generate a random 32-byte secret key as hex
fn random_secret() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:0>64x}", seed)
}

/// Test that link-visible repos (#k=secret) can be pushed and cloned with the secret
#[test]
fn test_link_visible_push_and_clone_with_secret() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19400);
    let server = match TestServer::new(19401) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found.");
            return;
        }
    };

    println!("\n=== Link-Visible Repo Test: Push and Clone with Secret ===\n");

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    let secret = random_secret();
    let remote_url = format!("htree://self/link-visible-test#k={}", secret);

    println!("Using secret: {}...", &secret[..16]);
    println!("Remote URL: {}", remote_url);

    // Add remote with secret
    Command::new("git")
        .args(["remote", "add", "htree", &remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    // Push
    println!("\nPushing link-visible repo...");
    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr = String::from_utf8_lossy(&push.stderr);
    println!("{}", stderr);
    assert!(
        push.status.success() || stderr.contains("-> master"),
        "Push should succeed"
    );

    // Clone with the same secret
    let npub = &test_env.npub;
    let clone_url = format!("htree://{}/link-visible-test#k={}", npub, secret);
    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("cloned");

    println!("\nCloning with correct secret...");
    let clone = Command::new("git")
        .args(["clone", &clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to clone");

    let clone_stderr = String::from_utf8_lossy(&clone.stderr);
    println!("{}", clone_stderr);
    assert!(clone.status.success(), "Clone with correct secret should succeed");

    // Verify files
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        std::fs::read_to_string(clone_path.join("README.md")).unwrap(),
        "Files should match"
    );

    println!("\n=== SUCCESS: Link-visible repo with secret works! ===");
}

/// Test that link-visible repos CANNOT be cloned without the secret
#[test]
fn test_link_visible_cannot_clone_without_secret() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19402);
    let server = match TestServer::new(19403) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found.");
            return;
        }
    };

    println!("\n=== Link-Visible Repo Test: Cannot Clone Without Secret ===\n");

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    let secret = random_secret();
    let remote_url = format!("htree://self/link-visible-noaccess#k={}", secret);

    println!("Pushing with secret: {}...", &secret[..16]);

    Command::new("git")
        .args(["remote", "add", "htree", &remote_url])
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

    assert!(
        push.status.success() || String::from_utf8_lossy(&push.stderr).contains("-> master"),
        "Push should succeed"
    );

    // Try to clone WITHOUT the secret (public URL)
    let npub = &test_env.npub;
    let public_clone_url = format!("htree://{}/link-visible-noaccess", npub);
    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("cloned-public");

    println!("\nTrying to clone without secret (should fail)...");
    let clone = Command::new("git")
        .args(["clone", &public_clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run clone");

    let clone_stderr = String::from_utf8_lossy(&clone.stderr);
    println!("{}", clone_stderr);

    // Clone should fail with a clear error about needing the secret
    let clone_failed = !clone.status.success()
        || clone_stderr.contains("link-visible")
        || clone_stderr.contains("requires a secret")
        || !clone_path.join("README.md").exists();

    assert!(
        clone_failed,
        "Clone without secret should fail with clear error. Stderr: {}",
        clone_stderr
    );

    // Verify we get a helpful error message (not just empty repo)
    assert!(
        clone_stderr.contains("link-visible") || clone_stderr.contains("secret"),
        "Should display helpful error about needing secret. Stderr: {}",
        clone_stderr
    );

    println!("\n=== SUCCESS: Cannot clone link-visible repo without secret! ===");
}

/// Test that link-visible repos CANNOT be cloned with wrong secret
#[test]
fn test_link_visible_cannot_clone_with_wrong_secret() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19404);
    let server = match TestServer::new(19405) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found.");
            return;
        }
    };

    println!("\n=== Link-Visible Repo Test: Cannot Clone With Wrong Secret ===\n");

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    let correct_secret = random_secret();
    let wrong_secret = random_secret(); // Different secret
    let remote_url = format!("htree://self/link-visible-wrongkey#k={}", correct_secret);

    println!("Pushing with correct secret: {}...", &correct_secret[..16]);
    println!("Will try cloning with wrong secret: {}...", &wrong_secret[..16]);

    Command::new("git")
        .args(["remote", "add", "htree", &remote_url])
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

    assert!(
        push.status.success() || String::from_utf8_lossy(&push.stderr).contains("-> master"),
        "Push should succeed"
    );

    // Try to clone with WRONG secret
    let npub = &test_env.npub;
    let wrong_clone_url = format!("htree://{}/link-visible-wrongkey#k={}", npub, wrong_secret);
    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("cloned-wrong");

    println!("\nTrying to clone with wrong secret (should fail)...");
    let clone = Command::new("git")
        .args(["clone", &wrong_clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run clone");

    let clone_stderr = String::from_utf8_lossy(&clone.stderr);
    println!("{}", clone_stderr);

    // With wrong secret, decryption will produce garbage, leading to parse errors
    // or hash mismatches. The clone should fail or produce corrupted data.
    let clone_failed_or_corrupted = !clone.status.success()
        || clone_stderr.contains("error")
        || clone_stderr.contains("fatal")
        || !clone_path.join("README.md").exists()
        || (clone_path.join("README.md").exists() && {
            // If file exists, content should NOT match (would be garbage)
            let original = std::fs::read_to_string(repo.path().join("README.md")).unwrap();
            let cloned = std::fs::read_to_string(clone_path.join("README.md")).unwrap_or_default();
            original != cloned
        });

    assert!(
        clone_failed_or_corrupted,
        "Clone with wrong secret should fail or produce corrupted data. Stderr: {}",
        clone_stderr
    );

    println!("\n=== SUCCESS: Cannot clone link-visible repo with wrong secret! ===");
}

/// Test that private repos (#private) can be pushed and cloned by the author
#[test]
fn test_private_push_and_clone_as_author() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19406);
    let server = match TestServer::new(19407) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found.");
            return;
        }
    };

    println!("\n=== Private Repo Test: Push and Clone as Author ===\n");

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    let remote_url = "htree://self/private-test#private";

    println!("Remote URL: {}", remote_url);

    // Add remote with #private
    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    // Push
    println!("\nPushing private repo...");
    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr = String::from_utf8_lossy(&push.stderr);
    println!("{}", stderr);
    assert!(
        push.status.success() || stderr.contains("-> master"),
        "Push should succeed"
    );

    // Clone as author (using self which resolves to same npub)
    let clone_url = "htree://self/private-test#private";
    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("cloned");

    println!("\nCloning as author with #private...");
    let clone = Command::new("git")
        .args(["clone", clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to clone");

    let clone_stderr = String::from_utf8_lossy(&clone.stderr);
    println!("{}", clone_stderr);
    assert!(clone.status.success(), "Clone as author should succeed");

    // Verify files
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        std::fs::read_to_string(clone_path.join("README.md")).unwrap(),
        "Files should match"
    );

    println!("\n=== SUCCESS: Private repo works for author! ===");
}

/// Test that private repos CANNOT be cloned without #private (as non-author)
#[test]
fn test_private_cannot_clone_without_private_flag() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19408);
    let server = match TestServer::new(19409) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found.");
            return;
        }
    };

    println!("\n=== Private Repo Test: Cannot Clone Without #private ===\n");

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    let remote_url = "htree://self/private-noaccess#private";

    // Push with #private
    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
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

    assert!(
        push.status.success() || String::from_utf8_lossy(&push.stderr).contains("-> master"),
        "Push should succeed"
    );

    // Try to clone WITHOUT #private (public URL)
    let npub = &test_env.npub;
    let public_clone_url = format!("htree://{}/private-noaccess", npub);
    let clone_dir = TempDir::new().unwrap();
    let clone_path = clone_dir.path().join("cloned-public");

    println!("\nTrying to clone without #private (should fail)...");
    let clone = Command::new("git")
        .args(["clone", &public_clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run clone");

    let clone_stderr = String::from_utf8_lossy(&clone.stderr);
    println!("{}", clone_stderr);

    // Clone should fail with an error about private repo
    let clone_failed = !clone.status.success()
        || clone_stderr.contains("private")
        || clone_stderr.contains("author")
        || !clone_path.join("README.md").exists();

    assert!(
        clone_failed,
        "Clone without #private should fail. Stderr: {}",
        clone_stderr
    );

    println!("\n=== SUCCESS: Cannot clone private repo without #private! ===");
}
