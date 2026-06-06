use super::*;

const TEST_PUBKEY: &str = "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";

fn test_config() -> Config {
    Config::default()
}

#[test]
fn test_new_client() {
    let config = test_config();
    let client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
    assert!(!client.relays.is_empty());
    assert!(!client.can_sign());
}

#[test]
fn test_new_client_with_secret() {
    let config = test_config();
    let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let client =
        NostrClient::new(TEST_PUBKEY, Some(secret.to_string()), None, false, &config).unwrap();
    assert!(client.can_sign());
}

#[test]
fn test_new_client_uses_local_read_server_as_daemon_fallback() {
    let mut config = test_config();
    config.server.bind_address = "127.0.0.1:1".to_string();
    config.blossom.read_servers = vec!["http://127.0.0.1:19092".to_string()];

    let client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
    assert_eq!(
        client.local_daemon_url.as_deref(),
        Some("http://127.0.0.1:19092")
    );
}

#[test]
fn test_new_client_uses_passed_blossom_config() {
    let mut config = test_config();
    config.server.bind_address = "127.0.0.1:1".to_string();
    config.blossom.servers.clear();
    config.blossom.read_servers = vec!["https://read.example".to_string()];
    config.blossom.write_servers = vec!["https://write.example".to_string()];

    let client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();

    assert!(client
        .blossom
        .read_servers()
        .contains(&"https://read.example".to_string()));
    assert!(client
        .blossom
        .read_servers()
        .contains(&"https://write.example".to_string()));
    assert_eq!(
        client.blossom.write_servers(),
        &["https://write.example".to_string()]
    );
}

#[test]
fn test_fetch_refs_empty() {
    let config = test_config();
    let client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
    let refs = client.cached_refs.get("new-repo");
    assert!(refs.is_none());
}

#[test]
fn test_validate_repo_publish_relays_allows_local_only_when_only_local_relays_configured() {
    let configured = vec!["ws://127.0.0.1:8080/ws".to_string()];
    let connected = vec!["ws://127.0.0.1:8080/ws".to_string()];

    assert!(validate_repo_publish_relays(&configured, &connected).is_ok());
}

#[test]
fn test_validate_repo_publish_relays_rejects_local_only_when_public_relays_configured() {
    let configured = vec![
        "ws://127.0.0.1:8080/ws".to_string(),
        "wss://relay.damus.io".to_string(),
    ];
    let connected = vec!["ws://127.0.0.1:8080/ws".to_string()];

    let err = validate_repo_publish_relays(&configured, &connected)
        .expect_err("should reject local-only publication");
    assert!(err.to_string().contains("No public relay confirmed"));
    assert!(err.to_string().contains("local relays only"));
}

#[test]
fn test_update_ref() {
    let config = test_config();
    let mut client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();

    client
        .update_ref("repo", "refs/heads/main", "abc123")
        .unwrap();

    let refs = client.cached_refs.get("repo").unwrap();
    assert_eq!(refs.get("refs/heads/main"), Some(&"abc123".to_string()));
}

#[test]
fn test_pick_latest_event_prefers_newer_timestamp() {
    let keys = Keys::generate();
    let older = Timestamp::from_secs(1_700_000_000);
    let newer = Timestamp::from_secs(1_700_000_001);

    let event_old = EventBuilder::new(Kind::Custom(KIND_APP_DATA), "old", [])
        .custom_created_at(older)
        .to_event(&keys)
        .unwrap();
    let event_new = EventBuilder::new(Kind::Custom(KIND_APP_DATA), "new", [])
        .custom_created_at(newer)
        .to_event(&keys)
        .unwrap();

    let picked = pick_latest_event([&event_old, &event_new]).unwrap();
    assert_eq!(picked.id, event_new.id);
}

#[test]
fn test_pick_latest_event_breaks_ties_with_event_id() {
    let keys = Keys::generate();
    let created_at = Timestamp::from_secs(1_700_000_000);

    let event_a = EventBuilder::new(Kind::Custom(KIND_APP_DATA), "a", [])
        .custom_created_at(created_at)
        .to_event(&keys)
        .unwrap();
    let event_b = EventBuilder::new(Kind::Custom(KIND_APP_DATA), "b", [])
        .custom_created_at(created_at)
        .to_event(&keys)
        .unwrap();

    let expected_id = if event_a.id > event_b.id {
        event_a.id
    } else {
        event_b.id
    };
    let picked = pick_latest_event([&event_a, &event_b]).unwrap();
    assert_eq!(picked.id, expected_id);
}

#[test]
fn test_next_replaceable_created_at_uses_now_when_existing_is_older() {
    let now = Timestamp::from_secs(1_700_000_010);
    let existing = Timestamp::from_secs(1_700_000_009);

    assert_eq!(
        next_replaceable_created_at(now, Some(existing)),
        now,
        "older repo events should not delay a new publish"
    );
}

#[test]
fn test_next_replaceable_created_at_bumps_same_second_events() {
    let now = Timestamp::from_secs(1_700_000_010);
    let existing = Timestamp::from_secs(1_700_000_010);

    assert_eq!(
        next_replaceable_created_at(now, Some(existing)),
        Timestamp::from_secs(1_700_000_011),
        "same-second repo publishes need a strictly newer timestamp"
    );
}

#[test]
fn test_pick_latest_repo_event_ignores_newer_different_d_tag() {
    let keys = Keys::generate();
    let older = Timestamp::from_secs(1_700_000_000);
    let newer = Timestamp::from_secs(1_700_000_031);

    let iris_chat = EventBuilder::new(
        Kind::Custom(KIND_APP_DATA),
        "good",
        [
            Tag::custom(TagKind::custom("d"), vec!["iris-chat".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
        ],
    )
    .custom_created_at(older)
    .to_event(&keys)
    .unwrap();

    let iris_chat_flutter = EventBuilder::new(
        Kind::Custom(KIND_APP_DATA),
        "bad",
        [
            Tag::custom(TagKind::custom("d"), vec!["iris-chat-flutter".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
        ],
    )
    .custom_created_at(newer)
    .to_event(&keys)
    .unwrap();

    let picked = pick_latest_repo_event([&iris_chat, &iris_chat_flutter], "iris-chat").unwrap();
    assert_eq!(picked.id, iris_chat.id);
}

#[test]
fn test_append_repo_discovery_labels_includes_git_label_and_prefixes() {
    let mut tags = vec![];
    append_repo_discovery_labels(&mut tags, "tools/hashtree");

    let values: Vec<String> = tags
        .iter()
        .filter_map(|tag| {
            let parts = tag.as_slice();
            if parts.first().map(|kind| kind.as_str()) != Some("l") {
                return None;
            }
            parts.get(1).cloned()
        })
        .collect();

    assert!(values.iter().any(|value| value == LABEL_GIT));
    assert!(values.iter().any(|value| value == "tools"));
}

#[test]
fn test_list_git_repo_announcements_filters_dedupes_and_sorts() {
    let keys = Keys::generate();
    let alpha_old = EventBuilder::new(
        Kind::Custom(KIND_APP_DATA),
        "old",
        [
            Tag::custom(TagKind::custom("d"), vec!["alpha".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_GIT.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&keys)
    .unwrap();
    let alpha_new = EventBuilder::new(
        Kind::Custom(KIND_APP_DATA),
        "new",
        [
            Tag::custom(TagKind::custom("d"), vec!["alpha".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_GIT.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(20))
    .to_event(&keys)
    .unwrap();
    let zeta = EventBuilder::new(
        Kind::Custom(KIND_APP_DATA),
        "zeta",
        [
            Tag::custom(TagKind::custom("d"), vec!["zeta/tools".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_GIT.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(15))
    .to_event(&keys)
    .unwrap();
    let ignored = EventBuilder::new(
        Kind::Custom(KIND_APP_DATA),
        "ignored",
        [
            Tag::custom(TagKind::custom("d"), vec!["not-git".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(30))
    .to_event(&keys)
    .unwrap();

    let repos = list_git_repo_announcements(&[alpha_old, zeta, ignored, alpha_new]);
    let names: Vec<&str> = repos.iter().map(|repo| repo.repo_name.as_str()).collect();

    assert_eq!(names, vec!["alpha", "zeta/tools"]);
    assert_eq!(repos[0].created_at, Timestamp::from_secs(20));
}

#[test]
fn test_parse_daemon_response_to_root_data_encrypted_key() {
    let payload = DaemonResolveResponse {
        hash: Some("ab".repeat(32)),
        cid: None,
        key: None,
        encrypted_key: Some("11".repeat(32)),
        self_encrypted_key: None,
        source: Some("webrtc".to_string()),
    };

    let parsed = NostrClient::parse_daemon_response_to_root_data(payload).unwrap();
    assert_eq!(parsed.root_hash, "ab".repeat(32));
    assert_eq!(parsed.key_tag_name.as_deref(), Some("encryptedKey"));
    assert!(parsed.self_encrypted_ciphertext.is_none());
    assert_eq!(parsed.encryption_key.unwrap(), [0x11; 32]);
}

#[test]
fn test_parse_daemon_response_to_root_data_self_encrypted() {
    let payload = DaemonResolveResponse {
        hash: Some("cd".repeat(32)),
        cid: None,
        key: None,
        encrypted_key: None,
        self_encrypted_key: Some("ciphertext".to_string()),
        source: Some("webrtc".to_string()),
    };

    let parsed = NostrClient::parse_daemon_response_to_root_data(payload).unwrap();
    assert_eq!(parsed.root_hash, "cd".repeat(32));
    assert_eq!(parsed.key_tag_name.as_deref(), Some("selfEncryptedKey"));
    assert_eq!(
        parsed.self_encrypted_ciphertext.as_deref(),
        Some("ciphertext")
    );
    assert!(parsed.encryption_key.is_none());
}

#[test]
fn test_parse_daemon_response_to_root_data_cid_key() {
    let payload = DaemonResolveResponse {
        hash: None,
        cid: Some(format!("{}:{}", "ab".repeat(32), "33".repeat(32))),
        key: None,
        encrypted_key: None,
        self_encrypted_key: None,
        source: Some("nostr".to_string()),
    };

    let parsed = NostrClient::parse_daemon_response_to_root_data(payload).unwrap();
    assert_eq!(parsed.root_hash, "ab".repeat(32));
    assert_eq!(parsed.key_tag_name, None);
    assert_eq!(parsed.encryption_key, Some([0x33; 32]));
}

#[tokio::test]
async fn test_fetch_root_from_local_daemon_parses_response() {
    use axum::{extract::Path, routing::get, Json, Router};
    use serde_json::json;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/api/nostr/resolve/:pubkey/:treename",
        get(
            |Path((pubkey, treename)): Path<(String, String)>| async move {
                Json(json!({
                    "key": format!("{}/{}", pubkey, treename),
                    "hash": "ab".repeat(32),
                    "source": "webrtc",
                    "key_tag": "22".repeat(32),
                }))
            },
        ),
    );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let config = test_config();
    let mut client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
    client.local_daemon_url = Some(format!("http://{}", addr));

    let resolved = client
        .fetch_root_from_local_daemon("repo", Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(resolved.root_hash, "ab".repeat(32));
    assert_eq!(resolved.key_tag_name.as_deref(), Some("key"));
    assert_eq!(resolved.encryption_key, Some([0x22; 32]));

    server.abort();
}

#[test]
fn test_fetch_refs_caches_root_when_tree_download_fails() {
    use std::io::{Read, Write};

    let root_hash = "ab".repeat(32);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_root_hash = root_hash.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = stream.read(&mut request);
        let body = serde_json::json!({
            "hash": server_root_hash,
            "source": "test",
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let mut config = test_config();
    config.nostr.relays.clear();
    let mut client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
    client.relays.clear();
    client.local_daemon_url = Some(format!("http://{}", addr));
    client.blossom = client.blossom.clone().with_read_servers(Vec::new());

    let err = client.fetch_refs_with_root("repo").unwrap_err();
    server.join().unwrap();

    assert!(
        err.to_string().contains("Failed to download root hash")
            || err.to_string().contains("No servers")
    );
    assert_eq!(client.get_cached_root_hash("repo"), Some(&root_hash));
    assert!(client.cached_refs.get("repo").is_none());
}

#[test]
fn test_stored_key_from_hex() {
    let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let key = StoredKey::from_secret_hex(secret, Some("test".to_string())).unwrap();
    assert_eq!(key.secret_hex.as_deref(), Some(secret));
    assert_eq!(key.petname, Some("test".to_string()));
    assert_eq!(key.pubkey_hex.len(), 64);
}

#[test]
fn test_stored_key_from_nsec() {
    let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
    let key = StoredKey::from_nsec(nsec, None).unwrap();
    assert_eq!(key.secret_hex.as_deref().map(str::len), Some(64));
    assert_eq!(key.pubkey_hex.len(), 64);
}

#[test]
fn test_stored_key_from_npub_is_read_only() {
    let npub = "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm";
    let key = StoredKey::from_npub(npub, Some("sirius".to_string())).unwrap();

    assert!(key.secret_hex.is_none());
    assert_eq!(key.petname.as_deref(), Some("sirius"));
    assert_eq!(key.pubkey_hex.len(), 64);
}

#[test]
fn test_resolve_self_identity_ignores_read_only_aliases() {
    let read_only = StoredKey::from_npub(
        "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm",
        Some("self".to_string()),
    )
    .unwrap();
    let signing = StoredKey::from_nsec(
        "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5",
        Some("work".to_string()),
    )
    .unwrap();

    let resolved = resolve_self_identity(&[read_only, signing.clone()]).unwrap();

    assert_eq!(resolved.0, signing.pubkey_hex);
    assert_eq!(resolved.1, signing.secret_hex);
}

#[test]
fn test_resolve_identity_hex_pubkey() {
    let result = resolve_identity(TEST_PUBKEY);
    assert!(result.is_ok());
    let (pubkey, secret) = result.unwrap();
    assert_eq!(pubkey, TEST_PUBKEY);
    assert!(secret.is_none());
}

#[test]
fn test_resolve_identity_npub() {
    let pk_bytes = hex::decode(TEST_PUBKEY).unwrap();
    let pk = PublicKey::from_slice(&pk_bytes).unwrap();
    let npub = pk.to_bech32().unwrap();

    let result = resolve_identity(&npub);
    assert!(result.is_ok(), "Failed: {:?}", result.err());
    let (pubkey, _) = result.unwrap();
    assert_eq!(pubkey.len(), 64);
    assert_eq!(pubkey, TEST_PUBKEY);
}

#[test]
fn test_format_repo_author_uses_full_npub() {
    let formatted = NostrClient::format_repo_author(TEST_PUBKEY);
    let expected = PublicKey::from_hex(TEST_PUBKEY)
        .unwrap()
        .to_bech32()
        .unwrap();

    assert_eq!(formatted, expected);
    assert!(!formatted.contains("..."));
}

#[test]
fn test_resolve_identity_unknown_petname() {
    let result = resolve_identity("nonexistent_petname_xyz");
    assert!(result.is_err());
}

#[test]
fn test_private_key_is_nip44_encrypted_not_plaintext() {
    use nostr_sdk::prelude::{nip44, Keys};

    let keys = Keys::generate();
    let pubkey = keys.public_key();

    let chk_key: [u8; 32] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
        0xcd, 0xef,
    ];
    let plaintext_hex = hex::encode(&chk_key);

    let encrypted = nip44::encrypt(
        keys.secret_key(),
        &pubkey,
        &plaintext_hex,
        nip44::Version::V2,
    )
    .expect("NIP-44 encryption should succeed");

    assert_ne!(
        encrypted, plaintext_hex,
        "NIP-44 encrypted value must differ from plaintext CHK hex"
    );
    assert!(
        !encrypted.contains(&plaintext_hex),
        "Encrypted value should not contain plaintext hex"
    );

    let decrypted = nip44::decrypt(keys.secret_key(), &pubkey, &encrypted)
        .expect("NIP-44 decryption should succeed");

    assert_eq!(
        decrypted, plaintext_hex,
        "Decrypted value should match original plaintext hex"
    );
}

#[test]
fn test_encryption_modes_produce_different_values() {
    use nostr_sdk::prelude::{nip44, Keys};

    let keys = Keys::generate();
    let pubkey = keys.public_key();

    let chk_key: [u8; 32] = [0xaa; 32];
    let plaintext_hex = hex::encode(&chk_key);

    let public_value = plaintext_hex.clone();
    let private_value = nip44::encrypt(
        keys.secret_key(),
        &pubkey,
        &plaintext_hex,
        nip44::Version::V2,
    )
    .expect("NIP-44 encryption should succeed");

    assert_ne!(
        private_value, public_value,
        "Private (NIP-44) value must differ from public (plaintext) value"
    );
    assert!(
        private_value.len() != 64,
        "NIP-44 output should not be 64 chars like hex CHK"
    );
}

fn build_test_pr_event(keys: &Keys, created_at_secs: u64) -> Event {
    EventBuilder::new(
        Kind::Custom(KIND_PULL_REQUEST),
        "",
        [Tag::custom(
            TagKind::custom("subject"),
            vec!["test pr".to_string()],
        )],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .to_event(keys)
    .unwrap()
}

fn build_test_status_event(
    keys: &Keys,
    kind: u16,
    pr_event_id: &str,
    created_at_secs: u64,
) -> Event {
    EventBuilder::new(
        Kind::Custom(kind),
        "",
        [Tag::custom(
            TagKind::custom("e"),
            vec![pr_event_id.to_string()],
        )],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .to_event(keys)
    .unwrap()
}

#[test]
fn test_pull_request_state_from_latest_status_kind_defaults_to_open() {
    assert_eq!(
        PullRequestState::from_latest_status_kind(None),
        PullRequestState::Open
    );
    assert_eq!(
        PullRequestState::from_latest_status_kind(Some(KIND_STATUS_OPEN)),
        PullRequestState::Open
    );
    assert_eq!(
        PullRequestState::from_latest_status_kind(Some(9999)),
        PullRequestState::Open
    );
}

#[test]
fn test_pull_request_state_from_status_kind_maps_known_kinds() {
    assert_eq!(
        PullRequestState::from_status_kind(KIND_STATUS_APPLIED),
        Some(PullRequestState::Applied)
    );
    assert_eq!(
        PullRequestState::from_status_kind(KIND_STATUS_CLOSED),
        Some(PullRequestState::Closed)
    );
    assert_eq!(
        PullRequestState::from_status_kind(KIND_STATUS_DRAFT),
        Some(PullRequestState::Draft)
    );
    assert_eq!(PullRequestState::from_status_kind(9999), None);
}

#[test]
fn test_pull_request_state_filter_includes_only_requested_state() {
    assert!(PullRequestStateFilter::Open.includes(PullRequestState::Open));
    assert!(!PullRequestStateFilter::Open.includes(PullRequestState::Closed));
    assert!(PullRequestStateFilter::All.includes(PullRequestState::Open));
    assert!(PullRequestStateFilter::All.includes(PullRequestState::Applied));
    assert!(PullRequestStateFilter::All.includes(PullRequestState::Closed));
    assert!(PullRequestStateFilter::All.includes(PullRequestState::Draft));
}

#[test]
fn test_pull_request_state_strings_are_stable() {
    assert_eq!(PullRequestState::Open.as_str(), "open");
    assert_eq!(PullRequestState::Applied.as_str(), "applied");
    assert_eq!(PullRequestState::Closed.as_str(), "closed");
    assert_eq!(PullRequestState::Draft.as_str(), "draft");

    assert_eq!(PullRequestStateFilter::Open.as_str(), "open");
    assert_eq!(PullRequestStateFilter::Applied.as_str(), "applied");
    assert_eq!(PullRequestStateFilter::Closed.as_str(), "closed");
    assert_eq!(PullRequestStateFilter::Draft.as_str(), "draft");
    assert_eq!(PullRequestStateFilter::All.as_str(), "all");
}

#[test]
fn test_latest_trusted_pr_status_kinds_ignores_untrusted_signers() {
    let repo_owner = Keys::generate();
    let pr_author = Keys::generate();
    let attacker = Keys::generate();

    let pr_event = build_test_pr_event(&pr_author, 1_700_100_000);
    let spoofed_status = build_test_status_event(
        &attacker,
        KIND_STATUS_CLOSED,
        &pr_event.id.to_hex(),
        1_700_100_010,
    );

    let statuses = latest_trusted_pr_status_kinds(
        &[pr_event.clone()],
        &[spoofed_status],
        &repo_owner.public_key().to_hex(),
    );

    assert!(
        !statuses.contains_key(&pr_event.id.to_hex()),
        "untrusted status signer should be ignored"
    );
}

#[test]
fn test_latest_trusted_pr_status_kinds_accepts_pr_author() {
    let repo_owner = Keys::generate();
    let pr_author = Keys::generate();

    let pr_event = build_test_pr_event(&pr_author, 1_700_100_000);
    let author_status = build_test_status_event(
        &pr_author,
        KIND_STATUS_CLOSED,
        &pr_event.id.to_hex(),
        1_700_100_010,
    );

    let statuses = latest_trusted_pr_status_kinds(
        &[pr_event.clone()],
        &[author_status],
        &repo_owner.public_key().to_hex(),
    );

    assert_eq!(
        statuses.get(&pr_event.id.to_hex()).copied(),
        Some(KIND_STATUS_CLOSED)
    );
}

#[test]
fn test_latest_trusted_pr_status_kinds_rejects_applied_from_pr_author() {
    let repo_owner = Keys::generate();
    let pr_author = Keys::generate();

    let pr_event = build_test_pr_event(&pr_author, 1_700_100_000);
    let author_applied = build_test_status_event(
        &pr_author,
        KIND_STATUS_APPLIED,
        &pr_event.id.to_hex(),
        1_700_100_010,
    );

    let statuses = latest_trusted_pr_status_kinds(
        &[pr_event.clone()],
        &[author_applied],
        &repo_owner.public_key().to_hex(),
    );

    assert!(
        !statuses.contains_key(&pr_event.id.to_hex()),
        "PR author must not be able to self-mark applied"
    );
}

#[test]
fn test_latest_trusted_pr_status_kinds_accepts_repo_owner() {
    let repo_owner = Keys::generate();
    let pr_author = Keys::generate();

    let pr_event = build_test_pr_event(&pr_author, 1_700_100_000);
    let owner_status = build_test_status_event(
        &repo_owner,
        KIND_STATUS_APPLIED,
        &pr_event.id.to_hex(),
        1_700_100_010,
    );

    let statuses = latest_trusted_pr_status_kinds(
        &[pr_event.clone()],
        &[owner_status],
        &repo_owner.public_key().to_hex(),
    );

    assert_eq!(
        statuses.get(&pr_event.id.to_hex()).copied(),
        Some(KIND_STATUS_APPLIED)
    );
}

#[test]
fn test_latest_trusted_pr_status_kinds_preserves_owner_applied_over_newer_author_status() {
    let repo_owner = Keys::generate();
    let pr_author = Keys::generate();

    let pr_event = build_test_pr_event(&pr_author, 1_700_100_000);
    let owner_applied = build_test_status_event(
        &repo_owner,
        KIND_STATUS_APPLIED,
        &pr_event.id.to_hex(),
        1_700_100_010,
    );
    let newer_author_open = build_test_status_event(
        &pr_author,
        KIND_STATUS_OPEN,
        &pr_event.id.to_hex(),
        1_700_100_020,
    );

    let statuses = latest_trusted_pr_status_kinds(
        &[pr_event.clone()],
        &[owner_applied, newer_author_open],
        &repo_owner.public_key().to_hex(),
    );

    assert_eq!(
        statuses.get(&pr_event.id.to_hex()).copied(),
        Some(KIND_STATUS_APPLIED),
        "owner-applied status should remain authoritative even if author publishes a newer status"
    );
}

#[test]
fn test_latest_trusted_pr_status_kinds_ignores_newer_untrusted_status() {
    let repo_owner = Keys::generate();
    let pr_author = Keys::generate();
    let attacker = Keys::generate();

    let pr_event = build_test_pr_event(&pr_author, 1_700_100_000);
    let trusted_open = build_test_status_event(
        &repo_owner,
        KIND_STATUS_OPEN,
        &pr_event.id.to_hex(),
        1_700_100_010,
    );
    let spoofed_closed = build_test_status_event(
        &attacker,
        KIND_STATUS_CLOSED,
        &pr_event.id.to_hex(),
        1_700_100_020,
    );

    let statuses = latest_trusted_pr_status_kinds(
        &[pr_event.clone()],
        &[trusted_open, spoofed_closed],
        &repo_owner.public_key().to_hex(),
    );

    assert_eq!(
        statuses.get(&pr_event.id.to_hex()).copied(),
        Some(KIND_STATUS_OPEN)
    );
}
