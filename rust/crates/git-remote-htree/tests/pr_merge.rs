//! Integration test for PR auto-merge detection on push
//!
//! Tests the workflow:
//! 1. Maintainer pushes repo to htree
//! 2. A kind 1618 PR event is published (simulating contributor's PR)
//! 3. Maintainer merges the branch and pushes
//! 4. Verify that a kind 1631 (STATUS_APPLIED) event is published

mod common;

use common::{
    create_test_repo, skip_if_no_binary,
    test_relay::{TestRelay, TestRelayOptions},
    TestEnv, TestServer,
};
use std::process::Command;
use std::time::Duration;

/// Test that pushing a merge commit auto-publishes a kind 1631 merged status event
#[test]
fn test_pr_auto_merge_detection() {
    if skip_if_no_binary() {
        return;
    }

    // Start local nostr relay and blossom server
    let relay = TestRelay::new(19500);
    println!("Started local nostr relay at: {}", relay.url());

    let server = match TestServer::new(19501) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };
    println!("Started local blossom server at: {}", server.base_url());

    println!("\n=== PR Auto-Merge Detection Test ===\n");

    // Create maintainer environment
    let maintainer_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let maintainer_npub = maintainer_env.npub.clone();
    println!("Maintainer: {}", &maintainer_npub[..20]);

    // Create test repo and push as maintainer
    let repo = create_test_repo();
    let repo_path = repo.path();
    let env_vars: Vec<_> = maintainer_env.env();

    // Add htree remote
    run_git(
        &["remote", "add", "htree", "htree://self/test-pr-merge"],
        repo_path,
        &env_vars,
    );

    // Push initial state
    println!("Pushing initial repo...");
    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo_path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git push");
    let stderr = String::from_utf8_lossy(&push.stderr);
    println!("Push stderr: {}", stderr);
    let push_worked = stderr.contains("-> master") || push.status.success();
    assert!(push_worked, "Initial push should succeed");

    // Create a feature branch with a commit
    run_git(&["checkout", "-b", "feature-branch"], repo_path, &env_vars);
    std::fs::write(repo_path.join("feature.txt"), "New feature\n").unwrap();
    run_git(&["add", "feature.txt"], repo_path, &env_vars);
    run_git(&["commit", "-m", "Add feature"], repo_path, &env_vars);

    // Get the commit tip of the feature branch
    let feature_tip = git_rev_parse("HEAD", repo_path, &env_vars);
    println!("Feature branch tip: {}", &feature_tip[..12]);

    // Go back to master
    run_git(&["checkout", "master"], repo_path, &env_vars);

    // Publish a fake kind 1618 PR event to the relay (simulating a contributor's PR)
    // We need the maintainer's pubkey hex for the 'a' tag
    let maintainer_pk =
        nostr::PublicKey::parse(&maintainer_npub).expect("Failed to parse maintainer npub");
    let maintainer_pubkey_hex = hex::encode(maintainer_pk.to_bytes());

    // Create a "contributor" key for the PR event
    let contributor_keys = nostr::Keys::generate();
    let contributor_pubkey_hex = hex::encode(contributor_keys.public_key().to_bytes());

    let repo_address = format!("30617:{}:test-pr-merge", maintainer_pubkey_hex);

    // Build and sign the PR event
    let pr_tags = vec![
        nostr::Tag::custom(nostr::TagKind::custom("a"), vec![repo_address]),
        nostr::Tag::custom(
            nostr::TagKind::custom("p"),
            vec![maintainer_pubkey_hex.clone()],
        ),
        nostr::Tag::custom(
            nostr::TagKind::custom("subject"),
            vec!["Add feature".to_string()],
        ),
        nostr::Tag::custom(
            nostr::TagKind::custom("branch"),
            vec!["feature-branch".to_string()],
        ),
        nostr::Tag::custom(
            nostr::TagKind::custom("target-branch"),
            vec!["master".to_string()],
        ),
        nostr::Tag::custom(nostr::TagKind::custom("c"), vec![feature_tip.clone()]),
    ];

    let pr_event = nostr::EventBuilder::new(nostr::Kind::Custom(1618), "")
        .tags(pr_tags)
        .sign_with_keys(&contributor_keys)
        .expect("Failed to build PR event");
    let pr_event_id = pr_event.id.to_hex();
    println!("PR event ID: {}", &pr_event_id[..12]);

    // Publish PR event to relay via websocket
    publish_event_to_relay(&relay.url(), &pr_event);
    println!("Published PR event to relay");

    // Now merge the feature branch into master (creating a merge commit)
    let merge = Command::new("git")
        .args([
            "merge",
            "feature-branch",
            "--no-ff",
            "-m",
            "Merge feature-branch",
        ])
        .current_dir(repo_path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git merge");
    assert!(
        merge.status.success(),
        "Merge should succeed: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    println!("Merged feature-branch into master");

    // Push the merge to htree — this should trigger auto-merge detection
    println!("\nPushing merge commit (should detect PR merge)...");
    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo_path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git push");
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    println!("Push stderr: {}", stderr2);

    let push2_worked = stderr2.contains("-> master") || push2.status.success();
    assert!(push2_worked, "Push with merge should succeed");

    // Check if the PR auto-merged message appeared in stderr
    let auto_merged = stderr2.contains("PR auto-merged:");
    println!("Auto-merge detected in output: {}", auto_merged);

    // Query relay for kind 1631 events referencing the PR event ID
    let status_events = query_relay_for_status(&relay.url(), &pr_event_id, &maintainer_pubkey_hex);

    println!(
        "Found {} kind 1631 status events for PR {}",
        status_events.len(),
        &pr_event_id[..12]
    );

    assert!(
        !status_events.is_empty(),
        "Should have published a kind 1631 merged status event for the PR"
    );

    // Verify the status event references the PR and contributor
    let status = &status_events[0];
    let has_e_tag = status
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|tags| {
            tags.iter().any(|tag| {
                tag.as_array()
                    .map(|arr| {
                        arr.len() >= 2
                            && arr[0].as_str() == Some("e")
                            && arr[1].as_str() == Some(&pr_event_id)
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    assert!(has_e_tag, "Status event should reference the PR event ID");

    let has_p_tag = status
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|tags| {
            tags.iter().any(|tag| {
                tag.as_array()
                    .map(|arr| {
                        arr.len() >= 2
                            && arr[0].as_str() == Some("p")
                            && arr[1].as_str() == Some(&contributor_pubkey_hex)
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    assert!(
        has_p_tag,
        "Status event should reference the contributor's pubkey"
    );

    println!("\n=== SUCCESS: PR auto-merge detection test passed! ===");
}

#[test]
fn test_pr_auto_merge_ignores_untrusted_spoofed_status() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19502);
    let server = match TestServer::new(19503) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let fixture = setup_pr_merge_fixture(&relay, &server, "test-pr-merge-spoof");

    let attacker_keys = nostr::Keys::generate();
    let spoofed_status = nostr::EventBuilder::new(nostr::Kind::Custom(1632), "")
        .tags(vec![
            nostr::Tag::custom(
                nostr::TagKind::custom("e"),
                vec![fixture.pr_event_id.clone()],
            ),
            nostr::Tag::custom(
                nostr::TagKind::custom("p"),
                vec![fixture.contributor_pubkey_hex.clone()],
            ),
        ])
        .custom_created_at(nostr::Timestamp::from_secs(1_800_000_000))
        .sign_with_keys(&attacker_keys)
        .expect("Failed to build spoofed status event");
    publish_event_to_relay(&relay.url(), &spoofed_status);

    let push2 = merge_feature_and_push(&fixture);
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    let push2_worked = stderr2.contains("-> master") || push2.status.success();
    assert!(push2_worked, "Push with merge should succeed");
    assert!(
        stderr2.contains("PR auto-merged:"),
        "Expected auto-merge success output after ignoring spoofed status.\nstderr:\n{}",
        stderr2
    );

    let status_events = query_relay_for_status(
        &relay.url(),
        &fixture.pr_event_id,
        &fixture.maintainer_pubkey_hex,
    );
    assert!(
        !status_events.is_empty(),
        "Maintainer merged status should be published despite spoofed status"
    );
}

#[test]
fn test_pr_auto_merge_does_not_report_success_when_status_publish_rejected() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::with_options(
        19504,
        TestRelayOptions {
            reject_event_kinds: vec![1631],
            ..Default::default()
        },
    );
    let server = match TestServer::new(19505) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let fixture = setup_pr_merge_fixture(&relay, &server, "test-pr-merge-reject-status");

    let push2 = merge_feature_and_push(&fixture);
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    let push2_worked = stderr2.contains("-> master") || push2.status.success();
    assert!(push2_worked, "Push with merge should succeed");
    assert!(
        !stderr2.contains("PR auto-merged:"),
        "Should not report auto-merge success when status publish is rejected.\nstderr:\n{}",
        stderr2
    );

    let status_events = query_relay_for_status(
        &relay.url(),
        &fixture.pr_event_id,
        &fixture.maintainer_pubkey_hex,
    );
    assert!(
        status_events.is_empty(),
        "No merged status event should be stored when relay rejects kind 1631"
    );
}

#[test]
fn test_pr_auto_merge_skips_when_pr_status_lookup_times_out() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::with_options(
        19506,
        TestRelayOptions {
            ignore_req_kinds: vec![1630, 1631, 1632, 1633],
            ..Default::default()
        },
    );
    let server = match TestServer::new(19507) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let fixture = setup_pr_merge_fixture(&relay, &server, "test-pr-merge-status-timeout");

    let maintainer_keys = load_test_env_keys(&fixture._maintainer_env);
    let preexisting_applied = nostr::EventBuilder::new(nostr::Kind::Custom(1631), "")
        .tags(vec![
            nostr::Tag::custom(
                nostr::TagKind::custom("e"),
                vec![fixture.pr_event_id.clone()],
            ),
            nostr::Tag::custom(
                nostr::TagKind::custom("p"),
                vec![fixture.contributor_pubkey_hex.clone()],
            ),
        ])
        .custom_created_at(nostr::Timestamp::from_secs(1_800_000_000))
        .sign_with_keys(&maintainer_keys)
        .expect("Failed to build maintainer applied status event");
    publish_event_to_relay(&relay.url(), &preexisting_applied);

    let before_count = count_stored_merged_status_events(
        &relay,
        &fixture.pr_event_id,
        &fixture.maintainer_pubkey_hex,
    );
    assert_eq!(
        before_count, 1,
        "Expected exactly one pre-seeded maintainer applied status event"
    );

    let push2 = merge_feature_and_push(&fixture);
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    let push2_worked = stderr2.contains("-> master") || push2.status.success();
    assert!(push2_worked, "Push with merge should succeed");
    assert!(
        !stderr2.contains("PR auto-merged:"),
        "Auto-merge publishing should be skipped when PR status lookup times out.\nstderr:\n{}",
        stderr2
    );

    let after_count = count_stored_merged_status_events(
        &relay,
        &fixture.pr_event_id,
        &fixture.maintainer_pubkey_hex,
    );
    assert_eq!(
        after_count, before_count,
        "Should not publish a duplicate merged status when status lookup fails"
    );
}

#[test]
fn test_pr_auto_merge_marks_multiple_prs_in_single_push() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19508);
    let server = match TestServer::new(19509) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let repo_name = "test-pr-merge-multi";
    let fixture = setup_pr_merge_fixture(&relay, &server, repo_name);

    merge_branch_no_ff(&fixture, "feature-branch", "Merge feature-branch");

    let second_pr = create_branch_and_publish_pr(
        &fixture,
        &relay,
        repo_name,
        "feature-branch-2",
        "feature2.txt",
        "Second feature\n",
        "Add feature 2",
    );
    merge_branch_no_ff(&fixture, &second_pr.branch, "Merge feature-branch-2");

    let push = push_master(&fixture);
    let stderr = String::from_utf8_lossy(&push.stderr);
    let push_worked = stderr.contains("-> master") || push.status.success();
    assert!(push_worked, "Push with two merges should succeed");

    assert_eq!(
        count_stored_merged_status_events(
            &relay,
            &fixture.pr_event_id,
            &fixture.maintainer_pubkey_hex
        ),
        1,
        "Expected exactly one merged status for the first PR"
    );
    assert_eq!(
        count_stored_merged_status_events(
            &relay,
            &second_pr.pr_event_id,
            &fixture.maintainer_pubkey_hex
        ),
        1,
        "Expected exactly one merged status for the second PR"
    );
}

#[test]
fn test_pr_auto_merge_only_marks_matching_pr_when_multiple_open_prs_exist() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19510);
    let server = match TestServer::new(19511) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let repo_name = "test-pr-merge-subset";
    let fixture = setup_pr_merge_fixture(&relay, &server, repo_name);

    let unmerged_pr = create_branch_and_publish_pr(
        &fixture,
        &relay,
        repo_name,
        "feature-branch-2",
        "feature2.txt",
        "Second feature\n",
        "Add feature 2",
    );

    merge_branch_no_ff(&fixture, "feature-branch", "Merge feature-branch");

    let push = push_master(&fixture);
    let stderr = String::from_utf8_lossy(&push.stderr);
    let push_worked = stderr.contains("-> master") || push.status.success();
    assert!(push_worked, "Push with one merged PR should succeed");

    assert_eq!(
        count_stored_merged_status_events(
            &relay,
            &fixture.pr_event_id,
            &fixture.maintainer_pubkey_hex
        ),
        1,
        "Merged PR should receive exactly one merged status"
    );
    assert_eq!(
        count_stored_merged_status_events(
            &relay,
            &unmerged_pr.pr_event_id,
            &fixture.maintainer_pubkey_hex
        ),
        0,
        "Unmerged PR must not receive a merged status"
    );
}

// --- Helper functions ---

struct PrMergeFixture {
    _maintainer_env: TestEnv,
    _repo: tempfile::TempDir,
    env_vars: Vec<(String, String)>,
    maintainer_pubkey_hex: String,
    contributor_pubkey_hex: String,
    pr_event_id: String,
}

struct PublishedPr {
    branch: String,
    pr_event_id: String,
}

impl PrMergeFixture {
    fn repo_path(&self) -> &std::path::Path {
        self._repo.path()
    }
}

fn setup_pr_merge_fixture(
    relay: &TestRelay,
    server: &TestServer,
    repo_name: &str,
) -> PrMergeFixture {
    let maintainer_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let maintainer_npub = maintainer_env.npub.clone();
    let env_vars: Vec<_> = maintainer_env.env();

    let repo = create_test_repo();
    let repo_path = repo.path();

    run_git(
        &[
            "remote",
            "add",
            "htree",
            &format!("htree://self/{repo_name}"),
        ],
        repo_path,
        &env_vars,
    );

    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo_path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run initial git push");
    let push_stderr = String::from_utf8_lossy(&push.stderr);
    let push_worked = push_stderr.contains("-> master") || push.status.success();
    assert!(push_worked, "Initial push should succeed");

    run_git(&["checkout", "-b", "feature-branch"], repo_path, &env_vars);
    std::fs::write(repo_path.join("feature.txt"), "New feature\n").unwrap();
    run_git(&["add", "feature.txt"], repo_path, &env_vars);
    run_git(&["commit", "-m", "Add feature"], repo_path, &env_vars);
    let feature_tip = git_rev_parse("HEAD", repo_path, &env_vars);
    run_git(&["checkout", "master"], repo_path, &env_vars);

    let maintainer_pk =
        nostr::PublicKey::parse(&maintainer_npub).expect("Failed to parse maintainer npub");
    let maintainer_pubkey_hex = hex::encode(maintainer_pk.to_bytes());

    let contributor_keys = nostr::Keys::generate();
    let contributor_pubkey_hex = hex::encode(contributor_keys.public_key().to_bytes());

    let repo_address = format!("30617:{}:{}", maintainer_pubkey_hex, repo_name);
    let pr_tags = vec![
        nostr::Tag::custom(nostr::TagKind::custom("a"), vec![repo_address]),
        nostr::Tag::custom(
            nostr::TagKind::custom("p"),
            vec![maintainer_pubkey_hex.clone()],
        ),
        nostr::Tag::custom(
            nostr::TagKind::custom("subject"),
            vec!["Add feature".to_string()],
        ),
        nostr::Tag::custom(
            nostr::TagKind::custom("branch"),
            vec!["feature-branch".to_string()],
        ),
        nostr::Tag::custom(
            nostr::TagKind::custom("target-branch"),
            vec!["master".to_string()],
        ),
        nostr::Tag::custom(nostr::TagKind::custom("c"), vec![feature_tip]),
    ];

    let pr_event = nostr::EventBuilder::new(nostr::Kind::Custom(1618), "")
        .tags(pr_tags)
        .sign_with_keys(&contributor_keys)
        .expect("Failed to build PR event");
    let pr_event_id = pr_event.id.to_hex();
    publish_event_to_relay(&relay.url(), &pr_event);

    PrMergeFixture {
        _maintainer_env: maintainer_env,
        _repo: repo,
        env_vars,
        maintainer_pubkey_hex,
        contributor_pubkey_hex,
        pr_event_id,
    }
}

fn merge_feature_and_push(fixture: &PrMergeFixture) -> std::process::Output {
    merge_branch_no_ff(fixture, "feature-branch", "Merge feature-branch");
    push_master(fixture)
}

fn merge_branch_no_ff(fixture: &PrMergeFixture, branch: &str, message: &str) {
    let repo_path = fixture.repo_path();

    let merge = Command::new("git")
        .args(["merge", branch, "--no-ff", "-m", message])
        .current_dir(repo_path)
        .envs(
            fixture
                .env_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed to run git merge");
    assert!(
        merge.status.success(),
        "Merge should succeed: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
}

fn push_master(fixture: &PrMergeFixture) -> std::process::Output {
    Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(fixture.repo_path())
        .envs(
            fixture
                .env_vars
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .output()
        .expect("Failed to run git push")
}

fn create_branch_and_publish_pr(
    fixture: &PrMergeFixture,
    relay: &TestRelay,
    repo_name: &str,
    branch: &str,
    filename: &str,
    file_contents: &str,
    subject: &str,
) -> PublishedPr {
    let repo_path = fixture.repo_path();

    run_git(&["checkout", "-b", branch], repo_path, &fixture.env_vars);
    std::fs::write(repo_path.join(filename), file_contents).unwrap();
    run_git(&["add", filename], repo_path, &fixture.env_vars);
    run_git(&["commit", "-m", subject], repo_path, &fixture.env_vars);
    let commit_tip = git_rev_parse("HEAD", repo_path, &fixture.env_vars);
    run_git(&["checkout", "master"], repo_path, &fixture.env_vars);

    let contributor_keys = nostr::Keys::generate();
    let repo_address = format!("30617:{}:{}", fixture.maintainer_pubkey_hex, repo_name);

    let pr_tags = vec![
        nostr::Tag::custom(nostr::TagKind::custom("a"), vec![repo_address]),
        nostr::Tag::custom(
            nostr::TagKind::custom("p"),
            vec![fixture.maintainer_pubkey_hex.clone()],
        ),
        nostr::Tag::custom(nostr::TagKind::custom("subject"), vec![subject.to_string()]),
        nostr::Tag::custom(nostr::TagKind::custom("branch"), vec![branch.to_string()]),
        nostr::Tag::custom(
            nostr::TagKind::custom("target-branch"),
            vec!["master".to_string()],
        ),
        nostr::Tag::custom(nostr::TagKind::custom("c"), vec![commit_tip]),
    ];

    let pr_event = nostr::EventBuilder::new(nostr::Kind::Custom(1618), "")
        .tags(pr_tags)
        .sign_with_keys(&contributor_keys)
        .expect("Failed to build PR event");
    let pr_event_id = pr_event.id.to_hex();
    publish_event_to_relay(&relay.url(), &pr_event);

    PublishedPr {
        branch: branch.to_string(),
        pr_event_id,
    }
}

fn load_test_env_keys(env: &TestEnv) -> nostr::Keys {
    let key_file = env.home_dir.join(".hashtree").join("keys");
    let key_text = std::fs::read_to_string(&key_file).expect("Failed to read test keys file");
    let nsec = key_text
        .split_whitespace()
        .next()
        .expect("Test keys file did not contain an nsec");
    let secret = nostr::SecretKey::parse(nsec).expect("Failed to parse test nsec");
    nostr::Keys::new(secret)
}

fn count_stored_merged_status_events(
    relay: &TestRelay,
    pr_event_id: &str,
    author_pubkey: &str,
) -> usize {
    relay
        .stored_events()
        .into_iter()
        .filter(|event| {
            event.get("kind").and_then(|v| v.as_u64()) == Some(1631)
                && event.get("pubkey").and_then(|v| v.as_str()) == Some(author_pubkey)
                && event
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array()
                                .map(|arr| {
                                    arr.len() >= 2
                                        && arr[0].as_str() == Some("e")
                                        && arr[1].as_str() == Some(pr_event_id)
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
        })
        .count()
}

fn run_git(args: &[&str], dir: &std::path::Path, env_vars: &[(String, String)]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .unwrap_or_else(|e| panic!("Failed to run git {}: {}", args[0], e));
    if !output.status.success() {
        panic!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn git_rev_parse(refspec: &str, dir: &std::path::Path, env_vars: &[(String, String)]) -> String {
    let output = Command::new("git")
        .args(["rev-parse", refspec])
        .current_dir(dir)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git rev-parse");
    assert!(output.status.success(), "git rev-parse failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Publish an event to the relay via websocket
fn publish_event_to_relay(relay_url: &str, event: &nostr::Event) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::{connect_async, tungstenite::Message};

        let (mut ws, _) = connect_async(relay_url)
            .await
            .expect("Failed to connect to relay");

        // Send EVENT message
        let event_json = serde_json::to_value(event).unwrap();
        let msg = serde_json::json!(["EVENT", event_json]);
        ws.send(Message::Text(msg.to_string())).await.unwrap();

        // Wait for OK response
        let timeout = tokio::time::timeout(Duration::from_secs(3), ws.next()).await;
        match timeout {
            Ok(Some(Ok(Message::Text(text)))) => {
                println!("Relay response: {}", text);
            }
            _ => {
                println!("No response from relay (timeout or error)");
            }
        }

        ws.close(None).await.ok();
    });
}

/// Query relay for kind 1631 status events referencing a PR event ID
fn query_relay_for_status(
    relay_url: &str,
    pr_event_id: &str,
    author_pubkey: &str,
) -> Vec<serde_json::Value> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::{connect_async, tungstenite::Message};

        let (mut ws, _) = connect_async(relay_url)
            .await
            .expect("Failed to connect to relay");

        // Send REQ for kind 1631 events with #e tag matching PR event ID
        let filter = serde_json::json!({
            "kinds": [1631],
            "authors": [author_pubkey],
            "#e": [pr_event_id]
        });
        let req = serde_json::json!(["REQ", "status-query", filter]);
        ws.send(Message::Text(req.to_string())).await.unwrap();

        let mut events = Vec::new();

        // Collect events until EOSE
        loop {
            let timeout = tokio::time::timeout(Duration::from_secs(3), ws.next()).await;
            match timeout {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let parsed: Vec<serde_json::Value> =
                        serde_json::from_str(&text).unwrap_or_default();
                    if parsed.is_empty() {
                        continue;
                    }
                    match parsed[0].as_str() {
                        Some("EVENT") if parsed.len() >= 3 => {
                            events.push(parsed[2].clone());
                        }
                        Some("EOSE") => break,
                        _ => continue,
                    }
                }
                _ => break,
            }
        }

        ws.close(None).await.ok();
        events
    })
}
