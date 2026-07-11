use super::*;

#[test]
fn local_only_daemon_query_outlives_its_collection_window() {
    assert_eq!(
        local_daemon_query_timeout(3, true),
        Duration::from_secs(LOCAL_DAEMON_QUERY_TIMEOUT_SECS)
    );
    assert_eq!(
        local_daemon_query_timeout(10, true),
        Duration::from_secs(10)
    );
    assert_eq!(
        local_daemon_query_timeout(10, false),
        Duration::from_secs(4)
    );
}

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

const TEST_PUBKEY: &str = "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";

fn test_config() -> Config {
    Config::default()
}

fn tag_has_values(tags: &[Tag], expected: &[&str]) -> bool {
    tags.iter().any(|tag| {
        let parts = tag.as_slice();
        if parts.len() < expected.len() {
            return false;
        }

        expected
            .iter()
            .enumerate()
            .all(|(index, value)| parts.get(index).is_some_and(|part| part == *value))
    })
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
fn test_local_daemon_only_requires_a_configured_daemon() {
    let mut config = test_config();
    config.server.bind_address = "127.0.0.1:0".to_string();
    config.blossom.servers.clear();
    config.blossom.read_servers.clear();
    config.blossom.write_servers.clear();

    let err = match NostrClient::new_with_local_daemon_only(
        TEST_PUBKEY,
        None,
        None,
        false,
        &config,
        true,
    ) {
        Ok(_) => panic!("strict mode must require a local daemon"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("HTREE_LOCAL_DAEMON_ONLY"));
    assert!(err.to_string().contains("running local htree daemon"));
}

#[tokio::test]
async fn test_local_daemon_only_keeps_root_and_data_requests_local() {
    use axum::{
        extract::{OriginalUri, Path, State},
        http::StatusCode,
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let local_requests = Arc::new(AtomicUsize::new(0));
    let local_app = Router::new()
        .route(
            "/api/nostr/resolve/:pubkey/:treename",
            get(
                |State(requests): State<Arc<AtomicUsize>>,
                 Path((_pubkey, _treename)): Path<(String, String)>,
                 OriginalUri(uri): OriginalUri| async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        uri.query().is_none(),
                        "local-only lookup must trust daemon cache"
                    );
                    Json(json!({
                        "hash": "ab".repeat(32),
                        "source": "local-relay",
                    }))
                },
            ),
        )
        .fallback(get(|State(requests): State<Arc<AtomicUsize>>| async move {
            requests.fetch_add(1, Ordering::SeqCst);
            StatusCode::NOT_FOUND
        }))
        .with_state(Arc::clone(&local_requests));
    let local_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local_listener.local_addr().unwrap();
    let local_server = tokio::spawn(async move {
        let _ = axum::serve(local_listener, local_app).await;
    });

    let public_requests = Arc::new(AtomicUsize::new(0));
    let public_app = Router::new()
        .fallback(get(|State(requests): State<Arc<AtomicUsize>>| async move {
            requests.fetch_add(1, Ordering::SeqCst);
            StatusCode::NOT_FOUND
        }))
        .with_state(Arc::clone(&public_requests));
    let public_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_addr = public_listener.local_addr().unwrap();
    let public_server = tokio::spawn(async move {
        let _ = axum::serve(public_listener, public_app).await;
    });

    let local_url = format!("http://{local_addr}");
    let public_url = format!("http://{public_addr}");
    let mut config = test_config();
    config.server.bind_address = "127.0.0.1:1".to_string();
    config.nostr.relays = vec![format!("ws://{public_addr}")];
    config.blossom.servers.clear();
    config.blossom.read_servers = vec![local_url.clone(), public_url];
    config.blossom.write_servers = vec![format!("http://{public_addr}")];

    let mut client =
        NostrClient::new_with_local_daemon_only(TEST_PUBKEY, None, None, false, &config, true)
            .unwrap();
    assert!(client.local_daemon_only());
    assert!(client.relays.is_empty());
    assert_eq!(client.blossom.read_servers(), &[local_url.clone()]);
    assert_eq!(client.blossom.write_servers(), &[local_url]);

    let err = tokio::task::spawn_blocking(move || client.fetch_refs_with_root("repo"))
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("local-daemon-only"));
    assert!(err.to_string().contains("fallback disabled"));
    assert!(local_requests.load(Ordering::SeqCst) >= 2);
    assert_eq!(public_requests.load(Ordering::SeqCst), 0);

    local_server.abort();
    public_server.abort();
}

#[tokio::test]
async fn test_local_daemon_only_publishes_signed_root_through_daemon_only() {
    use axum::{extract::State, http::StatusCode, routing::get, routing::post, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    let published = Arc::new(Mutex::new(None::<Event>));
    let legacy_cache_requests = Arc::new(AtomicUsize::new(0));
    let local_app =
        Router::new()
            .route(
                "/api/nostr/events",
                post(
                    |State(published): State<Arc<Mutex<Option<Event>>>>,
                     Json(event): Json<Event>| async move {
                        *published.lock().unwrap() = Some(event);
                        StatusCode::ACCEPTED
                    },
                ),
            )
            .route(
                "/api/cache-tree-root",
                post({
                    let requests = Arc::clone(&legacy_cache_requests);
                    move || async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    }
                }),
            )
            .fallback(
                get(|| async { StatusCode::NOT_FOUND }).post(|| async { StatusCode::NOT_FOUND }),
            )
            .with_state(Arc::clone(&published));
    let local_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local_listener.local_addr().unwrap();
    let local_server = tokio::spawn(async move {
        let _ = axum::serve(local_listener, local_app).await;
    });

    let public_requests = Arc::new(AtomicUsize::new(0));
    let public_app = Router::new()
        .fallback(get(|State(requests): State<Arc<AtomicUsize>>| async move {
            requests.fetch_add(1, Ordering::SeqCst);
            StatusCode::NOT_FOUND
        }))
        .with_state(Arc::clone(&public_requests));
    let public_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_addr = public_listener.local_addr().unwrap();
    let public_server = tokio::spawn(async move {
        let _ = axum::serve(public_listener, public_app).await;
    });

    let keys = Keys::generate();
    let mut config = test_config();
    config.server.bind_address = "127.0.0.1:1".to_string();
    config.nostr.relays = vec![format!("ws://{public_addr}")];
    config.blossom.servers.clear();
    config.blossom.read_servers = vec![format!("http://{local_addr}")];
    config.blossom.write_servers = vec![format!("http://{public_addr}")];
    let client = NostrClient::new_with_local_daemon_only(
        &keys.public_key().to_hex(),
        Some(hex::encode(keys.secret_key().to_secret_bytes())),
        None,
        false,
        &config,
        true,
    )
    .unwrap();

    let result =
        tokio::task::spawn_blocking(move || client.publish_repo("site", &"ab".repeat(32), None))
            .await
            .unwrap()
            .unwrap();
    assert!(result.1.configured.is_empty());
    assert!(result.1.connected.is_empty());
    let event = published.lock().unwrap().clone().expect("published event");
    event.verify().unwrap();
    assert_eq!(event.kind, Kind::Custom(KIND_HASHTREE_ROOT));
    assert!(event.tags.iter().any(|tag| {
        let values = tag.as_slice();
        values.first().is_some_and(|value| value == "d")
            && values.get(1).is_some_and(|value| value == "site")
    }));
    assert_eq!(legacy_cache_requests.load(Ordering::SeqCst), 0);
    assert_eq!(public_requests.load(Ordering::SeqCst), 0);

    local_server.abort();
    public_server.abort();
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

    let event_old = event_builder!(Kind::Custom(KIND_APP_DATA), "old")
        .custom_created_at(older)
        .sign_with_keys(&keys)
        .unwrap();
    let event_new = event_builder!(Kind::Custom(KIND_APP_DATA), "new")
        .custom_created_at(newer)
        .sign_with_keys(&keys)
        .unwrap();

    let picked = pick_latest_event([&event_old, &event_new]).unwrap();
    assert_eq!(picked.id, event_new.id);
}

#[test]
fn test_pick_latest_event_breaks_ties_with_event_id() {
    let keys = Keys::generate();
    let created_at = Timestamp::from_secs(1_700_000_000);

    let event_a = event_builder!(Kind::Custom(KIND_APP_DATA), "a")
        .custom_created_at(created_at)
        .sign_with_keys(&keys)
        .unwrap();
    let event_b = event_builder!(Kind::Custom(KIND_APP_DATA), "b")
        .custom_created_at(created_at)
        .sign_with_keys(&keys)
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

    let iris_chat = event_builder!(
        Kind::Custom(KIND_APP_DATA),
        "good",
        [
            Tag::custom(TagKind::custom("d"), vec!["iris-chat".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
        ],
    )
    .custom_created_at(older)
    .sign_with_keys(&keys)
    .unwrap();

    let iris_chat_flutter = event_builder!(
        Kind::Custom(KIND_APP_DATA),
        "bad",
        [
            Tag::custom(TagKind::custom("d"), vec!["iris-chat-flutter".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
        ],
    )
    .custom_created_at(newer)
    .sign_with_keys(&keys)
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
fn test_repo_announcement_tags_include_iris_git_web_url() {
    let tags = NostrClient::build_repo_announcement_tags(
        "tools/my repo",
        "htree://npub1owner/tools/my repo",
        &[],
        &RepoAnnouncementOptions::default(),
    );

    assert!(tag_has_values(
        &tags,
        &["clone", "htree://npub1owner/tools/my repo"]
    ));
    assert!(tag_has_values(
        &tags,
        &["web", "https://git.iris.to/#/npub1owner/tools/my%20repo"]
    ));
}

#[test]
fn test_repo_announcement_tags_skip_web_url_for_non_npub_clone_url() {
    let tags = NostrClient::build_repo_announcement_tags(
        "tools",
        "htree://self/tools",
        &[],
        &RepoAnnouncementOptions::default(),
    );

    assert!(!tag_has_values(&tags, &["web"]));
}

#[test]
fn test_list_git_repo_announcements_filters_dedupes_and_sorts() {
    let keys = Keys::generate();
    let alpha_old = event_builder!(
        Kind::Custom(KIND_APP_DATA),
        "old",
        [
            Tag::custom(TagKind::custom("d"), vec!["alpha".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_GIT.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&keys)
    .unwrap();
    let alpha_new = event_builder!(
        Kind::Custom(KIND_APP_DATA),
        "new",
        [
            Tag::custom(TagKind::custom("d"), vec!["alpha".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_GIT.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(20))
    .sign_with_keys(&keys)
    .unwrap();
    let zeta = event_builder!(
        Kind::Custom(KIND_APP_DATA),
        "zeta",
        [
            Tag::custom(TagKind::custom("d"), vec!["zeta/tools".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_GIT.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(15))
    .sign_with_keys(&keys)
    .unwrap();
    let ignored = event_builder!(
        Kind::Custom(KIND_APP_DATA),
        "ignored",
        [
            Tag::custom(TagKind::custom("d"), vec!["not-git".to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
        ],
    )
    .custom_created_at(Timestamp::from_secs(30))
    .sign_with_keys(&keys)
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
        created_at: Some(10),
        event_id: Some("aa".repeat(32)),
    };

    let parsed = NostrClient::parse_daemon_response_to_root_data(payload).unwrap();
    assert_eq!(parsed.root_hash, "ab".repeat(32));
    assert_eq!(parsed.key_tag_name.as_deref(), Some("encryptedKey"));
    assert!(parsed.self_encrypted_ciphertext.is_none());
    assert_eq!(parsed.encryption_key.unwrap(), [0x11; 32]);
    assert_eq!(parsed.daemon_source.as_deref(), Some("webrtc"));
    assert_eq!(parsed.event_created_at, Some(10));
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
        created_at: None,
        event_id: None,
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
        created_at: None,
        event_id: None,
    };

    let parsed = NostrClient::parse_daemon_response_to_root_data(payload).unwrap();
    assert_eq!(parsed.root_hash, "ab".repeat(32));
    assert_eq!(parsed.key_tag_name, None);
    assert_eq!(parsed.encryption_key, Some([0x33; 32]));
}

#[test]
fn test_daemon_local_relay_root_is_fallback_until_relay_checked() {
    let local = RootEventData {
        root_hash: "11".repeat(32),
        source: RootResolveSource::LocalDaemon,
        daemon_source: Some("local-relay".to_string()),
        event_created_at: Some(10),
        event_id: Some("11".repeat(32)),
        ..RootEventData::default()
    };
    let relay = RootEventData {
        root_hash: "22".repeat(32),
        source: RootResolveSource::Relay,
        event_created_at: Some(20),
        event_id: Some("22".repeat(32)),
        ..RootEventData::default()
    };

    assert!(NostrClient::daemon_root_needs_relay_confirmation(&local));
    let chosen = NostrClient::choose_newer_root_data(local, relay);
    assert_eq!(chosen.root_hash, "22".repeat(32));
}

#[test]
fn test_daemon_nostr_relay_root_can_be_used_immediately() {
    let local = RootEventData {
        root_hash: "11".repeat(32),
        source: RootResolveSource::LocalDaemon,
        daemon_source: Some("nostr-relay".to_string()),
        event_created_at: Some(20),
        event_id: Some("22".repeat(32)),
        ..RootEventData::default()
    };

    assert!(!NostrClient::daemon_root_needs_relay_confirmation(&local));
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
fn test_fetch_refs_does_not_cache_unverified_daemon_root_when_tree_download_fails() {
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
        err.to_string().contains("Repository 'repo' not found")
            || err.to_string().contains("Failed to download root hash")
            || err.to_string().contains("No servers")
    );
    assert_eq!(client.get_cached_root_hash("repo"), None);
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
    event_builder!(
        Kind::Custom(KIND_PULL_REQUEST),
        "",
        [Tag::custom(
            TagKind::custom("subject"),
            vec!["test pr".to_string()],
        )],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .sign_with_keys(keys)
    .unwrap()
}

fn build_test_status_event(
    keys: &Keys,
    kind: u16,
    pr_event_id: &str,
    created_at_secs: u64,
) -> Event {
    event_builder!(
        Kind::Custom(kind),
        "",
        [Tag::custom(
            TagKind::custom("e"),
            vec![pr_event_id.to_string()],
        )],
    )
    .custom_created_at(Timestamp::from_secs(created_at_secs))
    .sign_with_keys(keys)
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
