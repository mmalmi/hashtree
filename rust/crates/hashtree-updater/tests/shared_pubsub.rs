#![cfg(feature = "nostr-pubsub")]

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use hashtree_core::{to_hex, Cid, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
use hashtree_resolver::{Event, RootResolver};
use hashtree_updater::{
    HashtreeUpdater, PubsubRootResolver, UpdateCheckOptions, UpdateEventCache, UpdateRef,
    UpdateTarget,
};
use nostr_pubsub::{
    EventBus, EventSource, Filter, InMemoryEventBus, NostrEventHandler, NostrEventSubscriber,
    NostrEventSubscription, SubscriptionDeliveryStatus, VerifiedEvent,
};
use nostr_sdk::{EventBuilder, Keys, Kind, Tag, TagKind, Timestamp, ToBech32};
use tokio::sync::Notify;

#[cfg(feature = "peer-storage")]
#[tokio::test]
async fn downloaded_release_is_reshared_without_the_original_source() {
    use hashtree_core::{Store, StoreBlobRoute};
    use hashtree_updater::{
        build_secure_pubsub_store_updater, build_update_store, SecurePubsubBlossomConfig,
    };

    let publisher = Keys::generate();
    let origin = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(origin.clone()).public());
    let bytes = vec![42; 1024 * 1024 + 17];
    let manifest = br#"{"app":"app","version":"2.0.0","assets":[{"name":"app","path":"app","target":"linux-x86_64"}]}"#;
    let mut entries = Vec::new();
    for (name, data) in [
        ("release.json", manifest.as_slice()),
        ("app", bytes.as_slice()),
    ] {
        let (cid, size) = tree.put_file(data).await.unwrap();
        entries.push(
            DirEntry::from_cid(name, &cid)
                .with_size(size)
                .with_link_type(LinkType::File),
        );
    }
    let root = tree.put_directory(entries).await.unwrap();
    let reference = UpdateRef {
        npub: publisher.public_key().to_bech32().unwrap(),
        tree_name: "releases/app".into(),
        path: None,
    };
    let event = signed_root(&publisher, &reference.tree_name, 10, &root);
    let downloaded = Arc::new(MemoryStore::new());
    for (source, destination) in [
        (origin.clone(), downloaded.clone()),
        (downloaded.clone(), Arc::new(MemoryStore::new())),
    ] {
        let provider = Arc::new(SharedProvider::default());
        let store = build_update_store(
            destination.clone(),
            Some(Arc::new(StoreBlobRoute::new(source))),
            SecurePubsubBlossomConfig {
                blossom_read_servers: Vec::new(),
                manifest_timeout: Duration::from_millis(20),
                download_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let updater =
            build_secure_pubsub_store_updater(provider.clone(), store, Duration::from_millis(20));
        let check = updater.check(UpdateCheckOptions {
            reference: reference.clone(),
            current_version: "1.0.0".into(),
            target: UpdateTarget::new("linux-x86_64"),
            ..Default::default()
        });
        let announce = async {
            provider.started.notified().await;
            announce(
                &provider.bus,
                event.clone(),
                EventSource::peer("serving-peer"),
            )
            .await;
        };
        let (check, ()) = tokio::join!(check, announce);
        let check = check.unwrap();
        assert!(check.update_available);
        assert_eq!(
            updater.download_asset(&check, None).await.unwrap().bytes,
            bytes
        );
        assert!(destination.has(&root.hash).await.unwrap());
    }
}

#[cfg(feature = "peer-storage")]
#[tokio::test]
async fn corrupt_peer_blocks_are_neither_accepted_nor_cached() {
    use hashtree_core::{sha256, Store, StoreBlobRoute};
    use hashtree_updater::{build_update_store, SecurePubsubBlossomConfig};
    let peer = Arc::new(MemoryStore::new());
    let cache = Arc::new(MemoryStore::new());
    let hash = sha256(b"authentic release");
    peer.put(hash, b"substituted executable".to_vec())
        .await
        .unwrap();
    let store = build_update_store(
        cache.clone(),
        Some(Arc::new(StoreBlobRoute::new(peer))),
        SecurePubsubBlossomConfig::default(),
    )
    .unwrap();
    assert!(store.get(&hash).await.is_err());
    assert!(!cache.has(&hash).await.unwrap());
}

#[derive(Default)]
struct SharedProvider {
    bus: InMemoryEventBus,
    started: Notify,
    closed: Arc<AtomicUsize>,
    status: Arc<Mutex<Option<SubscriptionDeliveryStatus>>>,
}

#[async_trait]
impl NostrEventSubscriber for SharedProvider {
    async fn subscribe(
        &self,
        filters: Vec<Filter>,
        handler: NostrEventHandler,
    ) -> nostr_pubsub::Result<Box<dyn NostrEventSubscription>> {
        let subscription = self.bus.subscribe(filters, handler).await?;
        self.started.notify_one();
        Ok(Box::new(Subscription {
            inner: subscription,
            closed: self.closed.clone(),
            status: self.status.clone(),
        }))
    }
}

struct Subscription {
    inner: Box<dyn NostrEventSubscription>,
    closed: Arc<AtomicUsize>,
    status: Arc<Mutex<Option<SubscriptionDeliveryStatus>>>,
}

#[async_trait]
impl NostrEventSubscription for Subscription {
    fn delivery_status(&self) -> Option<SubscriptionDeliveryStatus> {
        *self.status.lock().unwrap()
    }

    async fn close(self: Box<Self>) -> nostr_pubsub::Result<()> {
        self.inner.close().await?;
        let mut status = self.status.lock().unwrap();
        if *status == Some(SubscriptionDeliveryStatus::Active) {
            *status = Some(SubscriptionDeliveryStatus::Closed);
        }
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn signed_root(keys: &Keys, tree: &str, at: u64, cid: &Cid) -> Event {
    EventBuilder::new(Kind::Custom(30064), "")
        .tags([
            Tag::identifier(tree),
            Tag::custom(TagKind::Custom("hash".into()), [to_hex(&cid.hash)]),
        ])
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .unwrap()
}

async fn announce(bus: &InMemoryEventBus, event: Event, source: EventSource) {
    bus.publish(VerifiedEvent::try_from(event).unwrap(), source)
        .await
        .unwrap();
}

#[tokio::test]
async fn shared_provider_checks_and_downloads_without_a_relay_client() {
    let provider = Arc::new(SharedProvider::default());
    let keys = Keys::generate();
    let reference = UpdateRef {
        npub: keys.public_key().to_bech32().unwrap(),
        tree_name: "releases/app".into(),
        path: None,
    };
    let tree = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())).public());
    let manifest = br#"{"app":"app","version":"2.0.0","assets":[{"name":"app","path":"app","target":"x86_64-unknown-linux-gnu"}]}"#;
    let mut entries = Vec::new();
    for (name, bytes) in [
        ("release.json", manifest.as_slice()),
        ("app", b"signed through the tree".as_slice()),
    ] {
        let (cid, size) = tree.put_file(bytes).await.unwrap();
        entries.push(
            DirEntry::from_cid(name, &cid)
                .with_size(size)
                .with_link_type(LinkType::File),
        );
    }
    let cid = tree.put_directory(entries).await.unwrap();
    let trusted = signed_root(&keys, &reference.tree_name, 2, &cid);
    // Prior observations never satisfy the next check, even when their stored
    // source names a peer. This bus does not replay queries into live subscribers.
    announce(
        &provider.bus,
        trusted.clone(),
        EventSource::peer("prior-peer"),
    )
    .await;
    let resolver = PubsubRootResolver::new(provider.clone(), Duration::from_millis(40));
    resolver.ingest_event(trusted.clone()).await.unwrap();
    let updater = HashtreeUpdater::new(resolver, tree);
    let options = UpdateCheckOptions {
        reference: reference.clone(),
        current_version: "1.0.0".into(),
        target: UpdateTarget::new("linux-x86_64"),
        ..Default::default()
    };
    assert!(updater
        .check(options.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("inconclusive"));
    provider.started.notified().await;
    let (result, ()) = tokio::join!(updater.check(options.clone()), async {
        provider.started.notified().await;
        announce(
            &provider.bus,
            signed_root(&Keys::generate(), &reference.tree_name, 3, &cid),
            EventSource::peer("wrong-publisher"),
        )
        .await;
        announce(
            &provider.bus,
            signed_root(&keys, "other/tree", 3, &cid),
            EventSource::peer("wrong-tree"),
        )
        .await;
        announce(
            &provider.bus,
            trusted.clone(),
            EventSource::fips_endpoint("fresh-peer"),
        )
        .await;
    });
    let check = result.unwrap();
    assert!(check.update_available);
    assert_eq!(check.root_cid, cid);
    assert_eq!(
        updater.download_asset(&check, None).await.unwrap().bytes,
        b"signed through the tree"
    );
    assert_eq!(
        updater
            .resolver()
            .latest_event(&reference.resolver_key())
            .await
            .unwrap(),
        Some(trusted)
    );
    assert_eq!(provider.closed.load(Ordering::SeqCst), 2);
    // The borrowed provider survives every check and still accepts other work.
    let (result, ()) = tokio::join!(updater.check(options), async {
        provider.started.notified().await;
        announce(
            &provider.bus,
            signed_root(&keys, &reference.tree_name, 1, &cid),
            EventSource::peer("stale-peer"),
        )
        .await;
    });
    assert!(result.unwrap_err().to_string().contains("inconclusive"));
}

#[tokio::test]
async fn cached_only_wrong_author_and_disconnected_provider_are_inconclusive() {
    let provider = Arc::new(SharedProvider::default());
    let keys = Keys::generate();
    let reference = UpdateRef {
        npub: keys.public_key().to_bech32().unwrap(),
        tree_name: "releases/app".into(),
        path: None,
    };
    let key = reference.resolver_key();
    let cid = Cid::public([7; 32]);
    let resolver = PubsubRootResolver::new(provider.clone(), Duration::from_millis(25));
    for (author, source) in [
        (&keys, EventSource::local_index("cache")),
        (&Keys::generate(), EventSource::peer("untrusted")),
    ] {
        let (result, ()) = tokio::join!(resolver.resolve(&key), async {
            provider.started.notified().await;
            announce(
                &provider.bus,
                signed_root(author, &reference.tree_name, 1, &cid),
                source,
            )
            .await;
        });
        assert!(result.unwrap_err().to_string().contains("inconclusive"));
    }
    assert!(resolver
        .resolve(&key)
        .await
        .unwrap_err()
        .to_string()
        .contains("inconclusive"));
    assert_eq!(provider.closed.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn interrupted_delivery_cannot_confirm_a_partial_peer_observation() {
    for status in [
        SubscriptionDeliveryStatus::Active,
        SubscriptionDeliveryStatus::Closed,
        SubscriptionDeliveryStatus::Lagged,
    ] {
        let provider = Arc::new(SharedProvider::default());
        *provider.status.lock().unwrap() = Some(SubscriptionDeliveryStatus::Active);
        let keys = Keys::generate();
        let key = format!("{}/release", keys.public_key().to_bech32().unwrap());
        let cid = Cid::public([9; 32]);
        let event = signed_root(&keys, "release", 1, &cid);
        let resolver = PubsubRootResolver::new(provider.clone(), Duration::from_millis(25));
        let (result, ()) = tokio::join!(resolver.resolve(&key), async {
            provider.started.notified().await;
            announce(
                &provider.bus,
                event.clone(),
                EventSource::peer("fresh-peer"),
            )
            .await;
            *provider.status.lock().unwrap() = Some(status);
        });
        if status == SubscriptionDeliveryStatus::Active {
            assert_eq!(result.unwrap(), Some(cid));
            assert_eq!(
                *provider.status.lock().unwrap(),
                Some(SubscriptionDeliveryStatus::Closed),
                "the resolver's own close must follow its health snapshot"
            );
        } else {
            assert!(result.unwrap_err().to_string().contains("inconclusive"));
        }
        assert_eq!(resolver.latest_event(&key).await.unwrap(), Some(event));
        assert_eq!(provider.closed.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn cache_rejects_tampering_and_uses_the_resolver_tie_break() {
    let keys = Keys::generate();
    let reference = UpdateRef {
        npub: keys.public_key().to_bech32().unwrap(),
        tree_name: "release".into(),
        path: None,
    };
    let mut events = [
        signed_root(&keys, "release", 10, &Cid::public([1; 32])),
        signed_root(&keys, "release", 10, &Cid::public([2; 32])),
    ];
    events.sort_by_key(|event| event.id);
    let mut cache = UpdateEventCache::new(&reference).unwrap();
    assert!(cache.ingest_event(events[1].clone()).unwrap());
    assert!(cache.ingest_event(events[0].clone()).unwrap());
    assert!(!cache.ingest_event(events[1].clone()).unwrap());
    let mut tampered = events[0].clone();
    tampered.content = "tampered".into();
    assert!(cache.ingest_event(tampered).is_err());
    assert_eq!(cache.latest().unwrap().as_event().id, events[0].id);
}

#[tokio::test]
async fn cancelling_a_check_closes_only_its_subscription() {
    let provider = Arc::new(SharedProvider::default());
    let keys = Keys::generate();
    let key = format!("{}/release", keys.public_key().to_bech32().unwrap());
    let resolver = PubsubRootResolver::new(provider.clone(), Duration::from_secs(60));
    let check = tokio::spawn(async move { resolver.resolve(&key).await });
    provider.started.notified().await;
    check.abort();
    let _ = check.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while provider.closed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled subscription closed");
    assert_eq!(provider.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replacing_a_provider_preserves_watermarks_and_uses_the_new_transport() {
    let original = Arc::new(SharedProvider::default());
    let replacement = Arc::new(SharedProvider::default());
    let resolver = PubsubRootResolver::new(original.clone(), Duration::from_millis(25));
    let keys = Keys::generate();
    let key = format!("{}/release", keys.public_key().to_bech32().unwrap());
    let cid = Cid::public([3; 32]);
    let event = signed_root(&keys, "release", 3, &cid);
    resolver.ingest_event(event.clone()).await.unwrap();
    let rebound = resolver.clone().with_provider(replacement.clone());
    assert_eq!(
        rebound.latest_event(&key).await.unwrap(),
        Some(event.clone())
    );
    let (result, ()) = tokio::join!(rebound.resolve(&key), async {
        tokio::time::timeout(Duration::from_secs(1), replacement.started.notified())
            .await
            .expect("replacement provider selected");
        announce(
            &replacement.bus,
            event,
            EventSource::peer("replacement-peer"),
        )
        .await;
    });
    assert_eq!(result.unwrap(), Some(cid));
    assert_eq!(original.closed.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.closed.load(Ordering::SeqCst), 1);
}
