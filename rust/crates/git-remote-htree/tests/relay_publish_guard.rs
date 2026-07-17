mod common;

use common::{
    command_output_with_timeout, create_test_repo, skip_if_no_binary, test_relay::TestRelay,
    TestEnv, TestServer,
};
use std::process::Command;
use std::time::Duration;

#[test]
fn test_git_push_fails_when_only_local_relay_confirms_publish() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19640);
    let server = match TestServer::new(19641) {
        Some(server) => server,
        None => {
            println!("SKIP: htree binary not found. Run `cargo build --bin htree` first.");
            return;
        }
    };

    let test_env = TestEnv::new(Some(&server.base_url()), Some(&relay.url()));
    let config_path = test_env.home_dir.join(".hashtree").join("config.toml");
    let config = format!(
        r#"
[server]
enable_auth = false

[nostr]
relays = ["{}", "ws://192.0.2.1:9"]

[blossom]
read_servers = ["{}"]
write_servers = ["{}"]
"#,
        relay.url(),
        server.base_url(),
        server.base_url()
    );
    std::fs::write(&config_path, config).expect("write test config");

    let repo = create_test_repo();
    let env_vars: Vec<_> = test_env.env();

    let add_remote = Command::new("git")
        .args([
            "remote",
            "add",
            "htree",
            "htree://self/local-only-publish-guard",
        ])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("add remote");
    assert!(
        add_remote.status.success(),
        "git remote add failed: {}",
        String::from_utf8_lossy(&add_remote.stderr)
    );

    let mut push = Command::new("git");
    push.args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let push = command_output_with_timeout(&mut push, Duration::from_secs(50))
        .expect("git push should honor the repository publication timeout");

    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        !push.status.success(),
        "git push should fail when only local relays confirm publish, stderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("local relays only")
            || stderr.contains("No public relay confirmed")
            || stderr.contains("publication timed out"),
        "stderr should explain the local-only relay publish failure, stderr:\n{}",
        stderr
    );
}
