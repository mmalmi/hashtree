//! Integration tests for `htree repos`

mod common;

use common::{htree_bin, write_keys_file};
use git_remote_htree::nostr_client::{KIND_APP_DATA, KIND_HASHTREE_ROOT};
use nostr::{Event, EventBuilder, Keys, Kind, Tag, TagKind, Timestamp, ToBech32};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

fn publish_event(relay_url: &str, event: &Event) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    rt.block_on(async move {
        let (mut ws, _) = connect_async(relay_url).await.expect("connect test relay");
        let msg = serde_json::json!(["EVENT", event]).to_string();
        ws.send(Message::Text(msg)).await.expect("send event");

        let response = ws.next().await.expect("relay ack").expect("ws frame");
        let response = match response {
            Message::Text(text) => text,
            other => panic!("unexpected relay response: {other:?}"),
        };
        let parsed: Vec<Value> = serde_json::from_str(&response).expect("parse relay response");
        assert_eq!(parsed.first().and_then(Value::as_str), Some("OK"));
        assert_eq!(parsed.get(2).and_then(Value::as_bool), Some(true));

        let _ = ws.close(None).await;
    });
}

fn build_repo_event_with_kind(
    author: &Keys,
    repo_name: &str,
    created_at_secs: u64,
    kind: u16,
) -> Event {
    event_builder!(
        Kind::Custom(kind),
        "ab".repeat(32),
        [
            Tag::custom(TagKind::custom("d"), vec![repo_name.to_string()]),
            Tag::custom(TagKind::custom("l"), vec!["hashtree".to_string()]),
            Tag::custom(TagKind::custom("l"), vec!["git".to_string()]),
            Tag::custom(TagKind::custom("hash"), vec!["ab".repeat(32)]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .sign_with_keys(author)
    .expect("build repo event")
}

fn build_repo_event(author: &Keys, repo_name: &str, created_at_secs: u64) -> Event {
    build_repo_event_with_kind(author, repo_name, created_at_secs, KIND_HASHTREE_ROOT)
}

fn build_legacy_repo_event(author: &Keys, repo_name: &str, created_at_secs: u64) -> Event {
    build_repo_event_with_kind(author, repo_name, created_at_secs, KIND_APP_DATA)
}

fn build_non_git_event(author: &Keys, repo_name: &str, created_at_secs: u64) -> Event {
    event_builder!(
        Kind::Custom(KIND_HASHTREE_ROOT),
        "cd".repeat(32),
        [
            Tag::custom(TagKind::custom("d"), vec![repo_name.to_string()]),
            Tag::custom(TagKind::custom("l"), vec!["hashtree".to_string()]),
            Tag::custom(TagKind::custom("hash"), vec!["cd".repeat(32)]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .sign_with_keys(author)
    .expect("build non-git event")
}

struct ReposFixture {
    relay: common::test_relay::TestRelay,
    tmp: TempDir,
    config_dir: PathBuf,
    data_dir: PathBuf,
    owner_keys: Keys,
    owner_npub: String,
}

impl ReposFixture {
    fn run_htree(&self, args: &[&str]) -> Output {
        Command::new(htree_bin())
            .env("HOME", self.tmp.path())
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .env("HTREE_DATA_DIR", &self.data_dir)
            .env("NOSTR_RELAYS", self.relay.url())
            .env("HTREE_PREFER_LOCAL_RELAY", "0")
            .args(args)
            .output()
            .expect("run htree")
    }

    fn publish(&self, event: &Event) {
        publish_event(&self.relay.url(), event);
    }
}

fn append_alias(config_dir: &Path, npub: &str, alias: &str) {
    let aliases_path = config_dir.join("aliases");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(aliases_path)
        .expect("open aliases file");
    writeln!(file, "{npub} {alias}").expect("append alias");
}

fn setup_fixture() -> ReposFixture {
    let relay = common::test_relay::TestRelay::new();
    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    let self_keys = Keys::generate();
    let self_nsec = self_keys.secret_key().to_bech32().expect("self nsec");
    write_keys_file(&config_dir, &self_nsec).expect("write keys file");

    let owner_keys = Keys::generate();
    let owner_npub = owner_keys.public_key().to_bech32().expect("owner npub");
    ReposFixture {
        relay,
        tmp,
        config_dir,
        data_dir,
        owner_keys,
        owner_npub,
    }
}

#[test]
fn test_repos_lists_git_repos_for_explicit_owner() {
    let fixture = setup_fixture();

    fixture.publish(&build_legacy_repo_event(
        &fixture.owner_keys,
        "zeta/tools",
        1_700_300_000,
    ));
    fixture.publish(&build_repo_event(
        &fixture.owner_keys,
        "alpha",
        1_700_300_010,
    ));
    fixture.publish(&build_non_git_event(
        &fixture.owner_keys,
        "not-a-git-repo",
        1_700_300_020,
    ));
    fixture.publish(&build_repo_event(
        &Keys::generate(),
        "someone-elses",
        1_700_300_030,
    ));

    let output = fixture.run_htree(&["repos", &fixture.owner_npub]);
    assert!(
        output.status.success(),
        "htree repos failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Git repos for"), "stdout:\n{stdout}");
    assert!(stdout.contains("htree://"), "stdout:\n{stdout}");
    assert!(stdout.contains("/alpha"), "stdout:\n{stdout}");
    assert!(stdout.contains("/zeta/tools"), "stdout:\n{stdout}");
    assert!(!stdout.contains("not-a-git-repo"), "stdout:\n{stdout}");
    assert!(!stdout.contains("someone-elses"), "stdout:\n{stdout}");

    let alpha_idx = stdout.find("/alpha").expect("alpha present");
    let zeta_idx = stdout.find("/zeta/tools").expect("zeta present");
    assert!(alpha_idx < zeta_idx, "stdout not sorted:\n{stdout}");
}

#[test]
fn test_repos_defaults_to_self() {
    let fixture = setup_fixture();
    let self_keys = Keys::new(self_keys_secret(&fixture));
    let self_npub = self_keys.public_key().to_bech32().expect("self npub");

    fixture.publish(&build_repo_event(&self_keys, "self-repo", 1_700_301_000));

    let output = fixture.run_htree(&["repos"]);
    assert!(
        output.status.success(),
        "htree repos without owner failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("htree://{self_npub}/self-repo")),
        "stdout:\n{stdout}"
    );
}

#[test]
fn test_repos_accepts_alias_owner() {
    let fixture = setup_fixture();
    append_alias(&fixture.config_dir, &fixture.owner_npub, "coworker");
    fixture.publish(&build_repo_event(
        &fixture.owner_keys,
        "alias-visible",
        1_700_302_000,
    ));

    let output = fixture.run_htree(&["repos", "coworker"]);
    assert!(
        output.status.success(),
        "htree repos coworker failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("/alias-visible"), "stdout:\n{stdout}");
    assert!(stdout.contains(&fixture.owner_npub), "stdout:\n{stdout}");
}

fn self_keys_secret(fixture: &ReposFixture) -> nostr::SecretKey {
    let keys_contents =
        fs::read_to_string(fixture.config_dir.join("keys")).expect("read keys file");
    let nsec = keys_contents
        .split_whitespace()
        .next()
        .expect("self nsec in keys file");
    nostr::SecretKey::parse(nsec).expect("parse self nsec")
}
