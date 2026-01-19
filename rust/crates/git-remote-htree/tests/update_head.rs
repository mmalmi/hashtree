//! Ensure fetch does not update the checked-out branch when using remote-tracking refs.

mod common;

use common::{create_test_repo, skip_if_no_binary, test_relay::TestRelay, TestEnv, TestServer};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git_rev_parse(dir: &Path, refname: &str) -> String {
    let output = Command::new("git")
        .args(["rev-parse", refname])
        .current_dir(dir)
        .output()
        .expect("Failed to run git rev-parse");
    assert!(
        output.status.success(),
        "git rev-parse {} failed: {}",
        refname,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn test_fetch_does_not_update_checked_out_branch() {
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

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    let head_before = git_rev_parse(repo.path(), "HEAD");

    let remote_url = "htree://self/update-head-test";
    let add_remote = Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");
    assert!(
        add_remote.status.success(),
        "git remote add failed: {}",
        String::from_utf8_lossy(&add_remote.stderr)
    );

    let push = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git push");
    assert!(
        push.status.success() || String::from_utf8_lossy(&push.stderr).contains("-> master"),
        "git push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    let clone_dir = TempDir::new().expect("Failed to create clone dir");
    let clone_path = clone_dir.path().join("cloned-repo");

    let clone = Command::new("git")
        .args([
            "clone",
            repo.path().to_str().unwrap(),
            clone_path.to_str().unwrap(),
        ])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git clone");
    assert!(
        clone.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );

    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&clone_path)
        .status()
        .expect("Failed to configure git");
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&clone_path)
        .status()
        .expect("Failed to configure git");

    std::fs::write(clone_path.join("new.txt"), "second commit\n").unwrap();
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(&clone_path)
        .status()
        .expect("Failed to git add");
    Command::new("git")
        .args(["commit", "-m", "Second commit"])
        .current_dir(&clone_path)
        .status()
        .expect("Failed to git commit");
    let pushed_sha = git_rev_parse(&clone_path, "HEAD");

    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(&clone_path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .status()
        .expect("Failed to add htree remote");

    let push2 = Command::new("git")
        .args(["push", "htree", "HEAD:master"])
        .current_dir(&clone_path)
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run second push");
    assert!(
        push2.status.success() || String::from_utf8_lossy(&push2.stderr).contains("-> master"),
        "second push failed: {}",
        String::from_utf8_lossy(&push2.stderr)
    );


    let set_fetch = Command::new("git")
        .args([
            "config",
            "--replace-all",
            "remote.htree.fetch",
            "+refs/heads/*:refs/remotes/htree/*",
        ])
        .current_dir(repo.path())
        .status()
        .expect("Failed to set fetch refspec");
    assert!(set_fetch.success(), "Failed to set fetch refspec");

    let fetch_specs = Command::new("git")
        .args(["config", "--get-all", "remote.htree.fetch"])
        .current_dir(repo.path())
        .output()
        .expect("Failed to read fetch refspec");
    assert!(
        fetch_specs.status.success(),
        "Failed to read fetch refspec: {}",
        String::from_utf8_lossy(&fetch_specs.stderr)
    );
    let specs_text = String::from_utf8_lossy(&fetch_specs.stdout);
    let specs: Vec<_> = specs_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(specs.len(), 1, "Expected a single fetch refspec");

    let fetch = Command::new("git")
        .args(["fetch", "htree"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to run git fetch");

    assert!(
        fetch.status.success(),
        "fetch should succeed with remote-tracking refspec: {}",
        String::from_utf8_lossy(&fetch.stderr)
    );

    let head_after = git_rev_parse(repo.path(), "HEAD");
    assert_eq!(head_before, head_after, "HEAD should remain unchanged");

    let remote_master = git_rev_parse(repo.path(), "refs/remotes/htree/master");
    assert_eq!(pushed_sha, remote_master, "remote tracking ref should update");
}
