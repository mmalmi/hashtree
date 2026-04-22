//! Diff-based push tests
//!
//! Tests that subsequent pushes only upload changed blobs.

mod common;

use common::{create_test_repo, skip_if_no_binary, test_relay::TestRelay, TestEnv, TestServer};
use std::process::{Command, Stdio};

#[derive(Debug, PartialEq, Eq)]
struct UploadProgress {
    processed: u32,
    total: u32,
    new_count: u32,
    unchanged: u32,
    exist: u32,
    failed: u32,
}

fn parse_upload_progress_line(line: &str) -> Option<UploadProgress> {
    let line = line.trim();
    let rest = line.strip_prefix("Uploading: ")?;
    let (progress, summary) = rest.split_once(" (")?;
    let (processed, total) = progress.split_once('/')?;
    let summary = summary.strip_suffix(')')?;

    let mut parsed = UploadProgress {
        processed: processed.parse().ok()?,
        total: total.parse().ok()?,
        new_count: 0,
        unchanged: 0,
        exist: 0,
        failed: 0,
    };

    for item in summary.split(", ") {
        let (count, label) = item.split_once(' ')?;
        let count: u32 = count.parse().ok()?;
        match label {
            "new" => parsed.new_count = count,
            "unchanged" => parsed.unchanged = count,
            "exist" => parsed.exist = count,
            "FAILED" => parsed.failed = count,
            _ => return None,
        }
    }

    Some(parsed)
}

fn parse_final_upload_progress(stderr: &str) -> Option<UploadProgress> {
    stderr
        .lines()
        .flat_map(|line| line.split('\r'))
        .filter_map(parse_upload_progress_line)
        .last()
}

fn parse_last_listed_object_count(stderr: &str) -> Option<u32> {
    stderr
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("Listing objects... ")?;
            let count = rest.split_whitespace().next()?;
            count.parse::<u32>().ok()
        })
        .last()
}

/// Test diff-based push - second push should upload fewer blobs
#[test]
fn test_diff_based_push() {
    if skip_if_no_binary() {
        return;
    }

    // Start local servers
    let relay = TestRelay::new(19202);
    let server = match TestServer::new(19203) {
        Some(s) => s,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    println!("=== Diff-Based Push Test ===\n");
    println!(
        "Local relay: {}, blossom: {}\n",
        relay.url(),
        server.base_url()
    );

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    // Create and push initial repo
    let repo = create_test_repo();
    println!("Test repo at: {:?}\n", repo.path());

    let remote_url = "htree://self/diff-test-repo";
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
    println!("First push stderr:\n{}", stderr1);

    if !push1.status.success() && !stderr1.contains("-> master") {
        panic!("First push failed: {}", stderr1);
    }
    let first_listed_objects =
        parse_last_listed_object_count(&stderr1).expect("first push should list objects");

    // Make a small change
    println!("\n=== Making small change ===");
    std::fs::write(
        repo.path().join("small-change.txt"),
        "Just a small change\n",
    )
    .expect("Failed to write file");

    Command::new("git")
        .args(["add", "small-change.txt"])
        .current_dir(repo.path())
        .output()
        .expect("Failed to git add");

    Command::new("git")
        .args(["commit", "-m", "Add small change"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .output()
        .expect("Failed to commit");

    // Second push - should use diff
    println!("\n=== Second push (should use diff) ===");
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

    // Verify diff was used
    let used_diff = stderr2.contains("unchanged") || stderr2.contains("Computing diff");
    println!("\nDiff optimization used: {}", used_diff);
    assert!(used_diff, "Second push should use diff optimization");

    let second_listed_objects =
        parse_last_listed_object_count(&stderr2).expect("second push should list objects");
    println!(
        "Object walk reduced: first={} second={}",
        first_listed_objects, second_listed_objects
    );
    assert!(
        second_listed_objects < first_listed_objects,
        "Second push should walk fewer git objects than the first push"
    );

    let final_progress = parse_final_upload_progress(&stderr2)
        .expect("Second push should print a final upload progress line");
    assert!(
        final_progress.unchanged > 0,
        "Second push should report unchanged objects in final progress: {:?}",
        final_progress
    );
    assert_eq!(
        final_progress.total,
        final_progress.new_count
            + final_progress.unchanged
            + final_progress.exist
            + final_progress.failed,
        "Final upload total should include unchanged/existing/failed objects: {:?}",
        final_progress
    );
    assert_eq!(
        final_progress.processed, final_progress.total,
        "Final upload progress should be complete: {:?}",
        final_progress
    );

    // Third push with no changes
    println!("\n=== Third push (no changes) ===");
    let push3 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to push");

    let stderr3 = String::from_utf8_lossy(&push3.stderr);
    println!("Third push stderr:\n{}", stderr3);

    // Either detects exact same root hash OR uploads very few blobs
    // Note: The index file contains timestamps which may cause small variations
    let no_changes = stderr3.contains("No changes") || stderr3.contains("same root");
    let minimal_upload = stderr3.contains("unchanged") && {
        // Parse "X new" from output - should be small (< 10)
        stderr3
            .split_whitespace()
            .zip(stderr3.split_whitespace().skip(1))
            .find(|(_, word)| *word == "new,")
            .and_then(|(num, _)| num.strip_prefix('(').unwrap_or(num).parse::<u32>().ok())
            .map(|n| n < 10)
            .unwrap_or(false)
    };
    println!(
        "No-change optimization used: {} (minimal_upload: {})",
        no_changes, minimal_upload
    );
    let third_listed_objects =
        parse_last_listed_object_count(&stderr3).expect("third push should list objects");
    println!("Third push listed objects: {}", third_listed_objects);
    assert!(
        third_listed_objects <= second_listed_objects,
        "Third push should not walk more git objects than the second push"
    );
    assert!(
        no_changes || minimal_upload,
        "Third push should detect no changes or upload minimal blobs"
    );

    println!("\n=== SUCCESS: Diff-based push test passed! ===");
}
