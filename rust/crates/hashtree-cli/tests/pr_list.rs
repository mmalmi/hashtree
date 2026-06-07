//! Integration tests for `htree pr list`
//!
//! Run with: cargo test --package hashtree-cli --test pr_list -- --nocapture

mod common;

use common::{htree_bin, run_git, write_keys_file};

use nostr::{Event, EventBuilder, Keys, Kind, Tag, TagKind, Timestamp, ToBech32};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
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

fn build_pr_event(
    author: &Keys,
    repo_address: &str,
    subject: &str,
    branch: &str,
    target_branch: &str,
    commit_tip: &str,
    created_at_secs: u64,
) -> Event {
    event_builder!(
        Kind::Custom(1618),
        "",
        [
            Tag::custom(TagKind::custom("a"), vec![repo_address.to_string()]),
            Tag::custom(TagKind::custom("subject"), vec![subject.to_string()]),
            Tag::custom(TagKind::custom("branch"), vec![branch.to_string()]),
            Tag::custom(
                TagKind::custom("target-branch"),
                vec![target_branch.to_string()],
            ),
            Tag::custom(TagKind::custom("c"), vec![commit_tip.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .sign_with_keys(author)
    .expect("build PR event")
}

fn build_status_event(
    signer: &Keys,
    pr_event_id: &str,
    status_kind: u16,
    created_at_secs: u64,
) -> Event {
    event_builder!(
        Kind::Custom(status_kind),
        "",
        [Tag::custom(
            TagKind::custom("e"),
            vec![pr_event_id.to_string()],
        )],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .sign_with_keys(signer)
    .expect("build status event")
}

struct PrListFixture {
    relay: common::test_relay::TestRelay,
    _tmp: TempDir,
    config_dir: PathBuf,
    data_dir: PathBuf,
    repo_dir: PathBuf,
    target_keys: Keys,
    target_npub: String,
    target_pubkey_hex: String,
    target_repo_name: String,
}

impl PrListFixture {
    fn target_repo_url(&self) -> String {
        format!("htree://{}/{}", self.target_npub, self.target_repo_name)
    }

    fn target_repo_address(&self) -> String {
        format!("30617:{}:{}", self.target_pubkey_hex, self.target_repo_name)
    }

    fn run_htree(&self, args: &[&str]) -> Output {
        self.run_htree_with_relay(args, &self.relay.url())
    }

    fn run_htree_with_relay(&self, args: &[&str], relay_url: &str) -> Output {
        Command::new(htree_bin())
            .current_dir(&self.repo_dir)
            .env("HOME", self._tmp.path())
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .env("HTREE_DATA_DIR", &self.data_dir)
            .env("NOSTR_RELAYS", relay_url)
            .env("HTREE_PREFER_LOCAL_RELAY", "0")
            .args(args)
            .output()
            .expect("run htree")
    }

    fn publish(&self, event: &Event) {
        publish_event(&self.relay.url(), event);
    }
}

fn setup_pr_list_fixture() -> PrListFixture {
    let relay = common::test_relay::TestRelay::new();

    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    let self_keys = Keys::generate();
    let self_nsec = self_keys
        .secret_key()
        .to_bech32()
        .expect("self nsec bech32");
    write_keys_file(&config_dir, &self_nsec).expect("write keys file");

    let target_keys = Keys::generate();
    let target_npub = target_keys.public_key().to_bech32().expect("target npub");
    let target_pubkey_hex = hex::encode(target_keys.public_key().to_bytes());

    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).expect("create repo dir");
    run_git(&repo_dir, &["init", "-b", "master"]);
    run_git(&repo_dir, &["config", "user.email", "test@example.com"]);
    run_git(&repo_dir, &["config", "user.name", "Test User"]);
    fs::write(repo_dir.join("README.md"), "# pr-list-test\n").expect("write readme");
    run_git(&repo_dir, &["add", "README.md"]);
    run_git(&repo_dir, &["commit", "-m", "initial"]);

    let target_repo_name = "target-repo".to_string();
    let target_repo_url = format!("htree://{}/{}", target_npub, target_repo_name);
    run_git(&repo_dir, &["remote", "add", "htree", &target_repo_url]);

    PrListFixture {
        relay,
        _tmp: tmp,
        config_dir,
        data_dir,
        repo_dir,
        target_keys,
        target_npub,
        target_pubkey_hex,
        target_repo_name,
    }
}

fn publish_open_pr(fixture: &PrListFixture, subject: &str, commit_tip: &str, created_at_secs: u64) {
    let repo_address = fixture.target_repo_address();
    let pr_author = Keys::generate();
    let pr_open = build_pr_event(
        &pr_author,
        &repo_address,
        subject,
        "feature/test",
        "master",
        commit_tip,
        created_at_secs,
    );
    fixture.publish(&pr_open);
}

fn assert_list_output_contains_subject(fixture: &PrListFixture, repo_arg: &str, subject: &str) {
    let output = fixture.run_htree(&["pr", "list", repo_arg]);
    assert!(
        output.status.success(),
        "htree pr list {repo_arg} failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(subject), "stdout:\n{stdout}");
}

fn append_key_alias(fixture: &PrListFixture, nsec: &str, alias: &str) {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(fixture.config_dir.join("keys"))
        .expect("open keys file for append");
    writeln!(file, "{nsec} {alias}").expect("append key alias");
}

fn unused_localhost_ws_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral localhost port");
    let port = listener
        .local_addr()
        .expect("get ephemeral listener address")
        .port();
    drop(listener);
    format!("ws://127.0.0.1:{port}")
}

#[test]
fn test_pr_list_defaults_to_open() {
    let fixture = setup_pr_list_fixture();
    let repo_address = fixture.target_repo_address();
    let pr_author = Keys::generate();
    let attacker = Keys::generate();

    let pr_open = build_pr_event(
        &pr_author,
        &repo_address,
        "Open PR",
        "feature/open",
        "master",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        1_700_200_000,
    );
    fixture.publish(&pr_open);

    let pr_closed = build_pr_event(
        &pr_author,
        &repo_address,
        "Closed PR",
        "feature/closed",
        "master",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        1_700_200_010,
    );
    fixture.publish(&pr_closed);
    fixture.publish(&build_status_event(
        &pr_author,
        &pr_closed.id.to_hex(),
        1632,
        1_700_200_020,
    ));

    let pr_applied = build_pr_event(
        &pr_author,
        &repo_address,
        "Applied PR",
        "feature/applied",
        "master",
        "cccccccccccccccccccccccccccccccccccccccc",
        1_700_200_030,
    );
    fixture.publish(&pr_applied);
    fixture.publish(&build_status_event(
        &fixture.target_keys,
        &pr_applied.id.to_hex(),
        1631,
        1_700_200_040,
    ));

    let pr_spoofed = build_pr_event(
        &pr_author,
        &repo_address,
        "Spoofed Status PR",
        "feature/spoofed",
        "master",
        "dddddddddddddddddddddddddddddddddddddddd",
        1_700_200_050,
    );
    fixture.publish(&pr_spoofed);
    fixture.publish(&build_status_event(
        &attacker,
        &pr_spoofed.id.to_hex(),
        1632,
        1_700_200_060,
    ));

    let output = fixture.run_htree(&["pr", "list", &fixture.target_repo_url()]);
    assert!(
        output.status.success(),
        "htree pr list failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Open PR"), "stdout:\n{stdout}");
    assert!(stdout.contains("Spoofed Status PR"), "stdout:\n{stdout}");
    assert!(!stdout.contains("Closed PR"), "stdout:\n{stdout}");
    assert!(!stdout.contains("Applied PR"), "stdout:\n{stdout}");
}

#[test]
fn test_pr_list_state_filters() {
    let fixture = setup_pr_list_fixture();
    let repo_address = fixture.target_repo_address();
    let pr_author = Keys::generate();

    let pr_open = build_pr_event(
        &pr_author,
        &repo_address,
        "Open PR",
        "feature/open",
        "master",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        1_700_201_000,
    );
    fixture.publish(&pr_open);

    let pr_closed = build_pr_event(
        &pr_author,
        &repo_address,
        "Closed PR",
        "feature/closed",
        "master",
        "ffffffffffffffffffffffffffffffffffffffff",
        1_700_201_010,
    );
    fixture.publish(&pr_closed);
    fixture.publish(&build_status_event(
        &pr_author,
        &pr_closed.id.to_hex(),
        1632,
        1_700_201_020,
    ));

    let pr_applied = build_pr_event(
        &pr_author,
        &repo_address,
        "Applied PR",
        "feature/applied",
        "master",
        "1111111111111111111111111111111111111111",
        1_700_201_030,
    );
    fixture.publish(&pr_applied);
    fixture.publish(&build_status_event(
        &fixture.target_keys,
        &pr_applied.id.to_hex(),
        1631,
        1_700_201_040,
    ));

    let pr_draft = build_pr_event(
        &pr_author,
        &repo_address,
        "Draft PR",
        "feature/draft",
        "master",
        "2222222222222222222222222222222222222222",
        1_700_201_050,
    );
    fixture.publish(&pr_draft);
    fixture.publish(&build_status_event(
        &pr_author,
        &pr_draft.id.to_hex(),
        1633,
        1_700_201_060,
    ));

    let output_all =
        fixture.run_htree(&["pr", "list", &fixture.target_repo_url(), "--state", "all"]);
    assert!(
        output_all.status.success(),
        "htree pr list --state all failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output_all.stdout),
        String::from_utf8_lossy(&output_all.stderr)
    );
    let stdout_all = String::from_utf8_lossy(&output_all.stdout);
    assert!(stdout_all.contains("Open PR"), "stdout:\n{stdout_all}");
    assert!(stdout_all.contains("Closed PR"), "stdout:\n{stdout_all}");
    assert!(stdout_all.contains("Applied PR"), "stdout:\n{stdout_all}");
    assert!(stdout_all.contains("Draft PR"), "stdout:\n{stdout_all}");

    let output_applied = fixture.run_htree(&[
        "pr",
        "list",
        &fixture.target_repo_url(),
        "--state",
        "applied",
    ]);
    assert!(
        output_applied.status.success(),
        "htree pr list --state applied failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output_applied.stdout),
        String::from_utf8_lossy(&output_applied.stderr)
    );
    let stdout_applied = String::from_utf8_lossy(&output_applied.stdout);
    assert!(
        stdout_applied.contains("Applied PR"),
        "stdout:\n{stdout_applied}"
    );
    assert!(
        !stdout_applied.contains("Open PR"),
        "stdout:\n{stdout_applied}"
    );
    assert!(
        !stdout_applied.contains("Closed PR"),
        "stdout:\n{stdout_applied}"
    );
    assert!(
        !stdout_applied.contains("Draft PR"),
        "stdout:\n{stdout_applied}"
    );
}

#[test]
fn test_pr_list_infers_repo_from_single_htree_remote() {
    let fixture = setup_pr_list_fixture();
    let repo_address = fixture.target_repo_address();
    let pr_author = Keys::generate();

    let pr_open = build_pr_event(
        &pr_author,
        &repo_address,
        "Inferred Repo PR",
        "feature/inferred",
        "master",
        "3333333333333333333333333333333333333333",
        1_700_202_000,
    );
    fixture.publish(&pr_open);

    let output = fixture.run_htree(&["pr", "list"]);
    assert!(
        output.status.success(),
        "htree pr list without repo failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Inferred Repo PR"), "stdout:\n{stdout}");
}

#[test]
fn test_pr_list_accepts_git_remote_alias() {
    let fixture = setup_pr_list_fixture();
    publish_open_pr(
        &fixture,
        "Alias PR",
        "4444444444444444444444444444444444444444",
        1_700_203_000,
    );

    assert_list_output_contains_subject(&fixture, "htree", "Alias PR");
}

#[test]
fn test_pr_list_accepts_git_remote_alias_with_slash() {
    let fixture = setup_pr_list_fixture();
    run_git(
        &fixture.repo_dir,
        &["remote", "add", "team/htree", &fixture.target_repo_url()],
    );

    publish_open_pr(
        &fixture,
        "Slash Alias PR",
        "4444444444444444444444444444444444444444",
        1_700_203_000,
    );

    assert_list_output_contains_subject(&fixture, "team/htree", "Slash Alias PR");
}

#[test]
fn test_pr_list_accepts_npub_repo_without_scheme() {
    let fixture = setup_pr_list_fixture();
    publish_open_pr(
        &fixture,
        "Npub Repo PR",
        "5555555555555555555555555555555555555555",
        1_700_204_000,
    );

    let target = format!("{}/{}", fixture.target_npub, fixture.target_repo_name);
    assert_list_output_contains_subject(&fixture, &target, "Npub Repo PR");
}

#[test]
fn test_pr_list_accepts_hex_pubkey_repo_without_scheme() {
    let fixture = setup_pr_list_fixture();
    publish_open_pr(
        &fixture,
        "Hex Repo PR",
        "6666666666666666666666666666666666666666",
        1_700_205_000,
    );

    let target = format!("{}/{}", fixture.target_pubkey_hex, fixture.target_repo_name);
    assert_list_output_contains_subject(&fixture, &target, "Hex Repo PR");
}

#[test]
fn test_pr_list_accepts_petname_repo_without_scheme() {
    let fixture = setup_pr_list_fixture();
    let target_nsec = fixture
        .target_keys
        .secret_key()
        .to_bech32()
        .expect("target nsec bech32");
    append_key_alias(&fixture, &target_nsec, "target");
    publish_open_pr(
        &fixture,
        "Petname Repo PR",
        "7777777777777777777777777777777777777777",
        1_700_206_000,
    );

    assert_list_output_contains_subject(&fixture, "target/target-repo", "Petname Repo PR");
}

#[test]
fn test_pr_list_accepts_htree_url_with_fragment() {
    let fixture = setup_pr_list_fixture();
    publish_open_pr(
        &fixture,
        "Fragment URL PR",
        "8888888888888888888888888888888888888888",
        1_700_207_000,
    );

    let target_with_fragment = format!("{}#k=super-secret", fixture.target_repo_url());
    let output = fixture.run_htree(&["pr", "list", &target_with_fragment]);
    assert!(
        output.status.success(),
        "htree pr list with fragment URL failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Fragment URL PR"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("super-secret") && !stderr.contains("super-secret"),
        "fragment leaked into output.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn test_pr_list_rejects_non_htree_remote_alias() {
    let fixture = setup_pr_list_fixture();
    run_git(
        &fixture.repo_dir,
        &[
            "remote",
            "add",
            "web-origin",
            "https://example.com/repo.git",
        ],
    );

    let output = fixture.run_htree(&["pr", "list", "web-origin"]);
    assert!(
        !output.status.success(),
        "expected non-htree alias to fail.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not an htree remote"),
        "stderr missing non-htree remote error.\nstderr:\n{stderr}"
    );
}

#[test]
fn test_pr_list_rejects_non_htree_remote_alias_with_slash() {
    let fixture = setup_pr_list_fixture();
    run_git(
        &fixture.repo_dir,
        &["remote", "add", "team/web", "https://example.com/repo.git"],
    );

    let output = fixture.run_htree(&["pr", "list", "team/web"]);
    assert!(
        !output.status.success(),
        "expected non-htree slash alias to fail.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not an htree remote"),
        "stderr missing non-htree remote error.\nstderr:\n{stderr}"
    );
}

#[test]
fn test_pr_list_prefers_git_remote_over_identity_repo_when_input_has_slash() {
    let fixture = setup_pr_list_fixture();
    let team_keys = Keys::generate();
    let team_nsec = team_keys
        .secret_key()
        .to_bech32()
        .expect("team nsec bech32");
    append_key_alias(&fixture, &team_nsec, "team");

    run_git(
        &fixture.repo_dir,
        &["remote", "add", "team/htree", &fixture.target_repo_url()],
    );
    publish_open_pr(
        &fixture,
        "Remote Wins PR",
        "9999999999999999999999999999999999999999",
        1_700_208_000,
    );

    assert_list_output_contains_subject(&fixture, "team/htree", "Remote Wins PR");
}

#[test]
fn test_pr_list_reports_empty_results() {
    let fixture = setup_pr_list_fixture();

    let output = fixture.run_htree(&["pr", "list", &fixture.target_repo_url()]);
    assert!(
        output.status.success(),
        "htree pr list failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No pull requests"),
        "expected empty-list message.\nstdout:\n{}",
        stdout
    );
}

#[test]
fn test_pr_list_includes_self_authored_browser_style_pr() {
    let fixture = setup_pr_list_fixture();
    let repo_address = fixture.target_repo_address();
    let clone_url = fixture.target_repo_url();

    let browser_style_pr = event_builder!(
        Kind::Custom(1618),
        "interop test PR",
        [
            Tag::custom(TagKind::custom("a"), vec![repo_address]),
            Tag::custom(
                TagKind::custom("p"),
                vec![fixture.target_pubkey_hex.clone()],
            ),
            Tag::custom(
                TagKind::custom("subject"),
                vec!["iris-git created PR".to_string()],
            ),
            Tag::custom(TagKind::custom("clone"), vec![clone_url]),
            Tag::custom(TagKind::custom("branch"), vec!["feature-ui".to_string()]),
            Tag::custom(TagKind::custom("target-branch"), vec!["master".to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(1_700_209_000))
    .sign_with_keys(&fixture.target_keys)
    .expect("build browser-style self PR");
    fixture.publish(&browser_style_pr);

    let output = fixture.run_htree(&["pr", "list", &fixture.target_repo_url()]);
    assert!(
        output.status.success(),
        "htree pr list failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("iris-git created PR"),
        "expected browser-style self-authored PR in output.\nstdout:\n{}",
        stdout
    );
}

#[test]
fn test_pr_list_surfaces_fetch_failure_instead_of_empty_results() {
    let fixture = setup_pr_list_fixture();
    let unreachable_relay = unused_localhost_ws_url();

    let output = fixture.run_htree_with_relay(
        &["pr", "list", &fixture.target_repo_url()],
        &unreachable_relay,
    );
    assert!(
        !output.status.success(),
        "expected pr list to fail when relay is unreachable.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("No pull requests found."),
        "stdout incorrectly reported empty PR list.\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("Failed to connect to any relay")
            || stderr.contains("Failed to fetch PR events from relays")
            || stderr.contains("Timed out fetching PR events from relays"),
        "stderr missing fetch/connect failure message.\nstderr:\n{stderr}"
    );
}
