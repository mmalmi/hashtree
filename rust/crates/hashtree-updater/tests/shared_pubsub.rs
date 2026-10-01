#![cfg(feature = "nostr-pubsub")]

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
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
    NostrEventSubscription, VerifiedEvent,
};
use nostr_sdk::{EventBuilder, Keys, Kind, Tag, TagKind, Timestamp, ToBech32};
use tokio::sync::Notify;

#[derive(Default)]
struct SharedProvider {
    bus: InMemoryEventBus,
    started: Notify,
    closed: Arc<AtomicUsize>,
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
        }))
    }
}

struct Subscription {
    inner: Box<dyn NostrEventSubscription>,
    closed: Arc<AtomicUsize>,
}

#[async_trait]
impl NostrEventSubscription for Subscription {
    async fn close(self: Box<Self>) -> nostr_pubsub::Result<()> {
        self.inner.close().await?;
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
