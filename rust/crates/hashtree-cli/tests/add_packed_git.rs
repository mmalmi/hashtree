#![cfg(feature = "git-remote-wrapper")]

// Use the same real relay, Blossom server and isolated identities as helper tests.
#[path = "../../git-remote-htree/tests/common/mod.rs"]
mod common;

use common::{command_output_with_timeout, test_relay::TestRelay, TestEnv, TestServer};
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

fn checked(command: &mut Command) -> Output {
    let output = command_output_with_timeout(command, Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(source: &Path, args: &[&str]) -> String {
    let output = checked(
        Command::new("git")
            .current_dir(source)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args),
    );
    String::from_utf8(output.stdout).unwrap()
}

fn objects(source: &Path) -> Vec<String> {
    let mut objects: Vec<_> = git(source, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    objects.sort();
    objects
}

#[test]
fn added_packed_repository_clones_with_ordinary_helper() {
    assert!(Path::new(env!("CARGO_BIN_EXE_htree")).is_file());
    assert!(Path::new(env!("CARGO_BIN_EXE_git-remote-htree")).is_file());
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main"]);
    std::fs::write(source.join("README.md"), "first revision\n").unwrap();
    git(&source, &["add", "README.md"]);
    git(&source, &["commit", "-m", "first"]);
    std::fs::write(source.join("README.md"), "second revision\n").unwrap();
    git(&source, &["commit", "-am", "second"]);
    git(&source, &["repack", "-ad"]);
    git(&source, &["prune-packed"]);
    git(&source, &["update-server-info"]);
    let pack_dir = source.join(".git/objects/pack");
    for extension in ["pack", "idx"] {
        assert!(std::fs::read_dir(&pack_dir).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            path.extension().is_some_and(|ext| ext == extension)
                && path.metadata().unwrap().len() > 0
        }));
    }
    assert!(
        std::fs::read_dir(source.join(".git/objects"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().len() == 2)
            .all(|entry| std::fs::read_dir(entry.path()).unwrap().next().is_none()),
        "fixture must require pack fetching, without loose-object fallback"
    );

    let relay = TestRelay::new();
    let server = TestServer::new().expect("newly built htree server is required");
    let publisher = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    checked(
        Command::new(env!("CARGO_BIN_EXE_htree"))
            .envs(publisher.env())
            .arg("--data-dir")
            .arg(publisher.home_dir.join("data"))
            .args(["add", "--no-ignore", "--publish", "packed-fixture"])
            .arg(&source),
    );

    // A different identity and empty store must fetch the actual uploaded pack.
    let reader = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let destination = temp.path().join("clone");
    checked(
        Command::new("git")
            .envs(reader.env())
            .arg("clone")
            .arg(format!("htree://{}/packed-fixture", publisher.npub))
            .arg(&destination),
    );
    assert_eq!(
        git(&destination, &["rev-parse", "HEAD"]),
        git(&source, &["rev-parse", "HEAD"])
    );
    assert_eq!(objects(&destination), objects(&source));
    assert_eq!(
        std::fs::read(destination.join("README.md")).unwrap(),
        b"second revision\n"
    );
    assert_eq!(
        git(&destination, &["rev-parse", "--is-shallow-repository"]),
        "false\n"
    );
    git(&destination, &["fsck", "--strict", "--full"]);
}
