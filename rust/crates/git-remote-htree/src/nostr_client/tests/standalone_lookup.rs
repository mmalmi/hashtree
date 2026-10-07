use super::*;
use hashtree_core::{nhash_encode_full, NHashData};
use hashtree_fs::FsBlobStore;
use hashtree_nostr::{NostrEventStore, VerifiedEvent};
use std::sync::Arc;

#[tokio::test]
async fn standalone_root_lookup_reads_offline_index_announcements_and_private_root() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("git-root-events");
    let store = Arc::new(FsBlobStore::new(cache.join("blobs")).unwrap());
    let events = NostrEventStore::new(store);
    let owner = Keys::generate();
    let indexer = Keys::generate();
    let root_hash = "42".repeat(32);
    let key = [0x73; 32];
    let encrypted = nip44::encrypt(
        owner.secret_key(),
        &owner.public_key(),
        hex::encode(key),
        nip44::Version::V2,
    )
    .unwrap();
    let root_event = EventBuilder::new(Kind::Custom(KIND_HASHTREE_ROOT), &root_hash)
        .tags([
            Tag::identifier("private-repo"),
            Tag::parse(["l", "hashtree"]).unwrap(),
            Tag::parse(["selfEncryptedKey", &encrypted]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .unwrap();
    let index_root = events
        .build(
            None,
            vec![VerifiedEvent::try_from(root_event)
                .unwrap()
                .to_stored_event()
                .into_stored()],
        )
        .await
        .unwrap()
        .unwrap();
    // Mirrors advertise only l=hashtree; the index publisher is not the repo owner.
    let mut tags = vec![
        Tag::identifier("nostr-event-index"),
        Tag::parse(["l", "hashtree"]).unwrap(),
        Tag::parse(["hash", &hex::encode(index_root.hash)]).unwrap(),
    ];
    if let Some(key) = index_root.key {
        tags.push(Tag::parse(["key", &hex::encode(key)]).unwrap());
    }
    let advertisement = EventBuilder::new(Kind::Custom(KIND_HASHTREE_ROOT), "")
        .tags(tags)
        .sign_with_keys(&indexer)
        .unwrap();
    let head = events
        .build(
            None,
            vec![VerifiedEvent::try_from(advertisement)
                .unwrap()
                .to_stored_event()
                .into_stored()],
        )
        .await
        .unwrap()
        .unwrap();
    std::fs::write(
        cache.join("head.nhash"),
        nhash_encode_full(&NHashData {
            hash: head.hash,
            decrypt_key: head.key,
        })
        .unwrap(),
    )
    .unwrap();
    let mut config = Config::default();
    config.storage.data_dir = directory.path().to_string_lossy().into_owned();
    config.nostr.relays.clear();
    config.blossom.read_servers.clear();
    config.blossom.servers.clear();
    config.blossom.write_servers.clear();
    let mut client = NostrClient::new_with_local_daemon_only(
        &owner.public_key().to_hex(),
        Some(owner.secret_key().to_secret_hex()),
        None,
        true,
        &config,
        false,
    )
    .unwrap();
    client.local_daemon_url = None;
    let resolved = tokio::time::timeout(
        Duration::from_secs(2),
        client.resolve_root_async_with_timeout("private-repo", 3, false),
    )
    .await
    .expect("offline lookup does not wait for relays")
    .expect("root exists in native index");
    assert_eq!(resolved.root_hash.as_deref(), Some(root_hash.as_str()));
    assert_eq!(resolved.encryption_key, Some(key));
}

#[tokio::test]
async fn unavailable_cached_index_is_an_incomplete_lookup_not_an_empty_repository() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("git-root-events");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("head.nhash"),
        nhash_encode_full(&NHashData {
            hash: [0x61; 32],
            decrypt_key: None,
        })
        .unwrap(),
    )
    .unwrap();
    let mut config = Config::default();
    config.storage.data_dir = directory.path().to_string_lossy().into_owned();
    config.blossom.servers.clear();
    config.blossom.read_servers.clear();
    config.blossom.write_servers.clear();
    let lookup = root_lookup::RootLookup::new(&config, None, false);
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        lookup.query(
            build_repo_event_filter(Keys::generate().public_key(), "missing"),
            &[],
            Duration::from_millis(50),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("Repository root observation incomplete"));
    assert!(error.downcast_ref::<RootObservationIncomplete>().is_some());
    assert!(error.downcast_ref::<RootNotObserved>().is_none());
}
