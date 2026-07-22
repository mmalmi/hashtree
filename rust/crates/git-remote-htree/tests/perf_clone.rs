//! Local clone benchmark for git-remote-htree.
//!
//! This is an ignored integration test so it can use the existing local relay and
//! blossom server harness while remaining easy to run from scripts.
//!
//! The benchmark is designed to be reusable for later transport changes such as
//! batch fetch or pack-based clone. It uses:
//!
//! - one publisher environment for seeding the remote
//! - one fresh consumer environment per clone iteration
//! - a real local relay + blossom server
//! - a real git clone over `htree://`

mod common;

use common::{skip_if_no_binary, test_relay::TestRelay, TestEnv, TestServer};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;
use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .parent()
        .expect("rust dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn benchmark_source_repo() -> PathBuf {
    std::env::var_os("HTREE_BENCH_SOURCE_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(workspace_root)
}

fn benchmark_iterations() -> usize {
    std::env::var("HTREE_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

fn benchmark_label() -> String {
    std::env::var("HTREE_BENCH_LABEL").unwrap_or_else(|_| "clone-local".to_string())
}

fn benchmark_repo_name() -> String {
    let suffix = std::process::id();
    std::env::var("HTREE_BENCH_REMOTE_REPO").unwrap_or_else(|_| format!("perf-clone-{}", suffix))
}

fn env_lookup(env_vars: &[(String, String)], key: &str) -> String {
    env_vars
        .iter()
        .find(|(env_key, _)| env_key == key)
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

fn run_git(path: &Path, env_vars: &[(String, String)], args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {:?}: {}", args, err))
}

fn assert_git_success(step: &str, output: &Output) {
    if !output.status.success() {
        panic!(
            "{} failed:\nstdout:\n{}\nstderr:\n{}",
            step,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn git_stdout(path: &Path, env_vars: &[(String, String)], args: &[&str]) -> String {
    let output = run_git(path, env_vars, args);
    assert_git_success(&format!("git {:?}", args), &output);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn clone_source_repo(source_repo: &Path) -> TempDir {
    let clone_dir = TempDir::new().expect("create source clone dir");
    let destination = clone_dir.path().join("source");
    let output = Command::new("git")
        .args([
            "clone",
            "--no-local",
            source_repo.to_str().expect("source repo path utf-8"),
            destination.to_str().expect("dest path utf-8"),
        ])
        .output()
        .expect("clone benchmark source repo");
    assert_git_success("clone benchmark source repo", &output);
    clone_dir
}

fn configure_repo_identity(repo_path: &Path) {
    assert_git_success(
        "git config user.email",
        &Command::new("git")
            .args(["config", "user.email", "bench@example.com"])
            .current_dir(repo_path)
            .output()
            .expect("git config user.email"),
    );
    assert_git_success(
        "git config user.name",
        &Command::new("git")
            .args(["config", "user.name", "Bench User"])
            .current_dir(repo_path)
            .output()
            .expect("git config user.name"),
    );
}

fn source_branch(repo_path: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_path)
        .output()
        .expect("resolve source branch");
    assert_git_success("git rev-parse --abbrev-ref HEAD", &output);
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        "master".to_string()
    } else {
        branch
    }
}

#[derive(Debug, Serialize)]
struct CloneRunMetrics {
    iteration: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
struct CloneBenchmarkResult {
    label: String,
    source_repo: String,
    remote_repo: String,
    source_branch: String,
    publisher_home: String,
    relay_url: String,
    blossom_url: String,
    clone_runs: Vec<CloneRunMetrics>,
    average_duration_ms: u128,
    min_duration_ms: u128,
    max_duration_ms: u128,
}

#[test]
#[ignore = "benchmark; run manually with cargo test --test perf_clone -- --ignored --nocapture"]
fn benchmark_large_repo_clone_local() {
    if skip_if_no_binary() {
        return;
    }

    let source_repo = benchmark_source_repo();
    let iterations = benchmark_iterations();
    let label = benchmark_label();
    let remote_repo = benchmark_repo_name();

    let relay = TestRelay::new();
    let server = TestServer::new().expect("start local blossom server");
    let relay_url = relay.url();
    let blossom_url = server.base_url();

    println!("Benchmark source repo: {}", source_repo.display());
    println!("Local relay: {}", relay_url);
    println!("Local blossom: {}", blossom_url);

    let source_clone_dir = clone_source_repo(&source_repo);
    let source_repo_path = source_clone_dir.path().join("source");
    configure_repo_identity(&source_repo_path);
    let branch = source_branch(&source_repo_path);

    let publisher_env = TestEnv::new(Some(&blossom_url), Some(&relay_url));
    let publisher_env_vars = publisher_env.env();

    let remote_url = format!("htree://self/{}", remote_repo);
    assert_git_success(
        "git remote add htree",
        &run_git(
            &source_repo_path,
            &publisher_env_vars,
            &["remote", "add", "htree", &remote_url],
        ),
    );

    let push_refspec = format!("{}:{}", branch, branch);
    let push_start = Instant::now();
    let push_output = run_git(
        &source_repo_path,
        &publisher_env_vars,
        &["push", "htree", &push_refspec],
    );
    let push_elapsed = push_start.elapsed();
    let push_stderr = String::from_utf8_lossy(&push_output.stderr);
    let push_worked = push_output.status.success()
        || push_stderr.contains(&format!("-> {}", branch))
        || push_stderr.contains("-> master")
        || push_stderr.contains("-> main");
    assert!(
        push_worked,
        "initial push failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&push_output.stdout),
        push_stderr
    );
    println!("Initial publish completed in {:?}", push_elapsed);

    let publisher_head = git_stdout(
        &source_repo_path,
        &publisher_env_vars,
        &["rev-parse", "HEAD"],
    );
    let clone_url = format!("htree://{}/{}", publisher_env.npub, remote_repo);

    let mut clone_runs = Vec::new();
    for iteration in 0..iterations {
        let consumer_env = TestEnv::new(Some(&blossom_url), Some(&relay_url));
        let consumer_env_vars = consumer_env.env();
        let consumer_home = env_lookup(&consumer_env_vars, "HOME");
        println!(
            "Iteration {} consumer home: {}",
            iteration + 1,
            consumer_home
        );

        let clone_dir = TempDir::new().expect("create clone dir");
        let clone_path = clone_dir.path().join("clone");

        let clone_start = Instant::now();
        let clone_output = Command::new("git")
            .args([
                "clone",
                &clone_url,
                clone_path.to_str().expect("clone path utf-8"),
            ])
            .envs(
                consumer_env_vars
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            )
            .output()
            .expect("run benchmark git clone");
        let clone_elapsed = clone_start.elapsed();

        assert!(
            clone_output.status.success(),
            "git clone failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&clone_output.stdout),
            String::from_utf8_lossy(&clone_output.stderr),
        );

        let cloned_head = git_stdout(&clone_path, &consumer_env_vars, &["rev-parse", "HEAD"]);
        assert_eq!(
            publisher_head, cloned_head,
            "cloned HEAD should match published HEAD"
        );

        println!(
            "Iteration {} clone completed in {:?}",
            iteration + 1,
            clone_elapsed
        );
        println!(
            "Iteration {} stderr:\n{}",
            iteration + 1,
            String::from_utf8_lossy(&clone_output.stderr)
        );

        clone_runs.push(CloneRunMetrics {
            iteration: iteration + 1,
            duration_ms: clone_elapsed.as_millis(),
        });
    }

    let total_duration_ms: u128 = clone_runs.iter().map(|run| run.duration_ms).sum();
    let average_duration_ms = total_duration_ms / clone_runs.len() as u128;
    let min_duration_ms = clone_runs
        .iter()
        .map(|run| run.duration_ms)
        .min()
        .unwrap_or_default();
    let max_duration_ms = clone_runs
        .iter()
        .map(|run| run.duration_ms)
        .max()
        .unwrap_or_default();

    let result = CloneBenchmarkResult {
        label,
        source_repo: source_repo.display().to_string(),
        remote_repo,
        source_branch: branch,
        publisher_home: publisher_env.home_dir.display().to_string(),
        relay_url,
        blossom_url,
        clone_runs,
        average_duration_ms,
        min_duration_ms,
        max_duration_ms,
    };

    let json = serde_json::to_string_pretty(&result).expect("serialize benchmark result");
    println!("HTREE_CLONE_BENCH_RESULT={}", json);

    if let Ok(output_path) = std::env::var("HTREE_BENCH_OUTPUT") {
        std::fs::write(&output_path, format!("{}\n", json))
            .unwrap_or_else(|err| panic!("write benchmark output {}: {}", output_path, err));
        println!("Wrote benchmark results to {}", output_path);
    }
}
