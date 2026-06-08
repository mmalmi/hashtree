//! Basic git push and clone tests
//!
//! Tests the fundamental git remote helper workflow:
//! - Push to htree://
//! - Clone from htree://
//! - Verify files match

mod common;

use common::{create_test_repo, skip_if_no_binary, test_relay::TestRelay, TestEnv, TestServer};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn run_git(args: &[&str], cwd: &Path, env_vars: &[(String, String)]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {:?}: {}", args, err))
}

fn assert_git_success(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_git_push_ok(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() || stderr.contains("-> master") || stderr.contains("-> main"),
        "git push failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
}

fn git_root_commit(repo_path: &Path, tip: &str) -> String {
    let output = Command::new("git")
        .args(["rev-list", "--max-parents=0", "--reverse", tip])
        .current_dir(repo_path)
        .output()
        .expect("failed to run git rev-list");
    assert_git_success(&["rev-list", "--max-parents=0", "--reverse", tip], &output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .expect("repo should have a root commit")
        .to_string()
}

fn event_has_tag(event: &Value, expected: &[&str]) -> bool {
    event
        .get("tags")
        .and_then(|tags| tags.as_array())
        .is_some_and(|tags| {
            tags.iter().any(|tag| {
                let Some(parts) = tag.as_array() else {
                    return false;
                };
                if parts.len() < expected.len() {
                    return false;
                }
                expected.iter().enumerate().all(|(index, value)| {
                    parts
                        .get(index)
                        .and_then(|part| part.as_str())
                        .is_some_and(|part| part == *value)
                })
            })
        })
}

fn find_event_by_kind_and_d(events: &[Value], kind: u64, d_tag: &str) -> Option<Value> {
    events
        .iter()
        .filter(|event| event.get("kind").and_then(|value| value.as_u64()) == Some(kind))
        .filter(|event| event_has_tag(event, &["d", d_tag]))
        .max_by_key(|event| {
            (
                event
                    .get("created_at")
                    .and_then(|value| value.as_u64())
                    .unwrap_or_default(),
                event
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .cloned()
}

fn wait_for_event_by_kind_and_d(relay: &TestRelay, kind: u64, d_tag: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = relay.stored_events();
        if let Some(event) = find_event_by_kind_and_d(&events, kind, d_tag) {
            return event;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for kind {} event with d={}.\nstored events:\n{}",
                kind,
                d_tag,
                serde_json::to_string_pretty(&events).unwrap_or_else(|_| format!("{events:?}"))
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

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
        panic!(
            "git clone failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
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
fn test_public_push_publishes_nip34_repo_announcement() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19640);
    let server = match TestServer::new(19641) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();
    let repo = create_test_repo();
    let repo_name = "nip34-public-push";

    let add_remote = run_git(
        &[
            "remote",
            "add",
            "htree",
            &format!("htree://self/{repo_name}"),
        ],
        repo.path(),
        &env_vars,
    );
    assert_git_success(
        &["remote", "add", "htree", "htree://self/<repo>"],
        &add_remote,
    );

    let push = run_git(&["push", "htree", "master"], repo.path(), &env_vars);
    assert_git_push_ok(&push);

    let root_commit = git_root_commit(repo.path(), "HEAD");
    let root_event = wait_for_event_by_kind_and_d(&relay, 30078, repo_name);
    assert!(event_has_tag(&root_event, &["l", "hashtree"]));
    assert!(event_has_tag(&root_event, &["l", "git"]));

    let repo_announcement = wait_for_event_by_kind_and_d(&relay, 30617, repo_name);
    assert!(event_has_tag(&repo_announcement, &["name", repo_name]));
    assert!(event_has_tag(
        &repo_announcement,
        &["clone", &format!("htree://{}/{repo_name}", test_env.npub)]
    ));
    assert!(event_has_tag(
        &repo_announcement,
        &["r", &root_commit, "euc"]
    ));
    assert!(
        !event_has_tag(&repo_announcement, &["t", "personal-fork"]),
        "plain public push should not be marked as a personal fork"
    );
}

#[test]
fn test_public_push_from_cloned_htree_repo_announces_personal_fork() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19642);
    let server = match TestServer::new(19643) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    let source_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let fork_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let source_env_vars: Vec<_> = source_env.env();
    let fork_env_vars: Vec<_> = fork_env.env();
    let source_repo = create_test_repo();
    let source_repo_name = "nip34-source-repo";
    let fork_repo_name = "nip34-fork-repo";

    let add_source_remote = run_git(
        &[
            "remote",
            "add",
            "htree",
            &format!("htree://self/{source_repo_name}"),
        ],
        source_repo.path(),
        &source_env_vars,
    );
    assert_git_success(
        &["remote", "add", "htree", "htree://self/<source>"],
        &add_source_remote,
    );
    let push_source = run_git(
        &["push", "htree", "master"],
        source_repo.path(),
        &source_env_vars,
    );
    assert_git_push_ok(&push_source);
    wait_for_event_by_kind_and_d(&relay, 30617, source_repo_name);

    let clone_dir = TempDir::new().expect("Failed to create clone dir");
    let clone_path = clone_dir.path().join("clone");
    let source_url = format!("htree://{}/{source_repo_name}", source_env.npub);
    let clone = Command::new("git")
        .args(["clone", &source_url, clone_path.to_str().unwrap()])
        .envs(fork_env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("failed to run git clone");
    assert_git_success(&["clone", "<source>", "<clone>"], &clone);

    assert_git_success(
        &["config", "user.name", "Fork User"],
        &run_git(
            &["config", "user.name", "Fork User"],
            &clone_path,
            &fork_env_vars,
        ),
    );
    assert_git_success(
        &["config", "user.email", "fork@example.com"],
        &run_git(
            &["config", "user.email", "fork@example.com"],
            &clone_path,
            &fork_env_vars,
        ),
    );
    let add_fork_remote = run_git(
        &[
            "remote",
            "add",
            "fork",
            &format!("htree://self/{fork_repo_name}"),
        ],
        &clone_path,
        &fork_env_vars,
    );
    assert_git_success(
        &["remote", "add", "fork", "htree://self/<fork>"],
        &add_fork_remote,
    );

    let push_fork = run_git(&["push", "fork", "master"], &clone_path, &fork_env_vars);
    assert_git_push_ok(&push_fork);

    let source_euc = git_root_commit(source_repo.path(), "HEAD");
    let fork_announcement = wait_for_event_by_kind_and_d(&relay, 30617, fork_repo_name);
    assert!(event_has_tag(
        &fork_announcement,
        &[
            "clone",
            &format!("htree://{}/{fork_repo_name}", fork_env.npub)
        ]
    ));
    assert!(event_has_tag(
        &fork_announcement,
        &["r", &source_euc, "euc"]
    ));
    assert!(event_has_tag(&fork_announcement, &["t", "personal-fork"]));
    assert!(event_has_tag(
        &fork_announcement,
        &[
            "forked-from",
            &format!("htree://{}/{source_repo_name}", source_env.npub)
        ]
    ));
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

#[test]
fn test_git_push_clone_and_pull_across_two_clients_local() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19310);
    let server = match TestServer::new(19311) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let author_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let consumer_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let author_env_fresh = TestEnv::with_nsec(
        Some(&server.base_url()),
        Some(&relay.url()),
        &author_env.nsec,
    );
    let author_env_vars: Vec<_> = author_env.env();
    let consumer_env_vars: Vec<_> = consumer_env.env();
    let author_env_fresh_vars: Vec<_> = author_env_fresh.env();

    let repo = create_test_repo();
    let remote_url = "htree://self/test-repo-pull-roundtrip";
    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(
            author_env_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed to add remote");

    let push1 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(
            author_env_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed first push");
    let stderr1 = String::from_utf8_lossy(&push1.stderr);
    assert!(
        push1.status.success() || stderr1.contains("-> master") || stderr1.contains("-> main"),
        "first push failed: {}",
        stderr1
    );

    let clone_url = format!("htree://{}/test-repo-pull-roundtrip", author_env.npub);
    let clone_dir = TempDir::new().expect("Failed to create clone dir");
    let clone_path = clone_dir.path().join("clone");
    let clone = Command::new("git")
        .args(["clone", &clone_url, clone_path.to_str().unwrap()])
        .envs(
            consumer_env_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed clone");
    assert!(
        clone.status.success(),
        "initial clone failed:\n{}",
        String::from_utf8_lossy(&clone.stderr)
    );

    let configure_clone = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&clone_path)
            .output()
            .expect("failed to configure clone repo");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    configure_clone(&["config", "user.name", "Test User"]);
    configure_clone(&["config", "user.email", "test@example.com"]);

    std::fs::write(repo.path().join("shared.txt"), "version 2\n").expect("write shared.txt");
    Command::new("git")
        .args(["add", "shared.txt"])
        .current_dir(repo.path())
        .output()
        .expect("git add shared.txt");
    Command::new("git")
        .args(["commit", "-m", "Add shared file"])
        .current_dir(repo.path())
        .output()
        .expect("git commit shared.txt");

    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(
            author_env_fresh_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed second push");
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    assert!(
        push2.status.success() || stderr2.contains("-> master") || stderr2.contains("-> main"),
        "second push failed: {}",
        stderr2
    );

    let pull = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&clone_path)
        .envs(
            consumer_env_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed pull");
    assert!(
        pull.status.success(),
        "git pull failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&pull.stdout),
        String::from_utf8_lossy(&pull.stderr)
    );

    assert_eq!(
        std::fs::read_to_string(clone_path.join("shared.txt")).unwrap(),
        "version 2\n"
    );
    let author_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("Failed to read author HEAD");
    let consumer_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&clone_path)
        .output()
        .expect("Failed to read consumer HEAD");
    assert_eq!(
        String::from_utf8_lossy(&author_head.stdout).trim(),
        String::from_utf8_lossy(&consumer_head.stdout).trim(),
        "consumer clone should fast-forward to author's pushed HEAD"
    );
}
