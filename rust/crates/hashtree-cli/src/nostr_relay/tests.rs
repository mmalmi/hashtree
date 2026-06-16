use super::*;
use anyhow::Result;
use nostr::{EventBuilder, Filter, JsonUtil, Keys, Kind, RelayMessage, SubscriptionId};
use std::collections::HashSet;
use tempfile::TempDir;
use tokio::time::{timeout, Duration};

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

async fn recv_relay_message(rx: &mut mpsc::UnboundedReceiver<String>) -> Result<RelayMessage<'_>> {
    let msg = timeout(Duration::from_secs(1), rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("channel closed"))?;
    Ok(RelayMessage::from_json(msg)?)
}

#[tokio::test]
async fn relay_stores_and_serves_events() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let mut allowed = HashSet::new();
    allowed.insert(keys.public_key().to_hex());
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        allowed,
    ));

    let relay_config = NostrRelayConfig {
        spambox_db_max_bytes: 0,
        ..Default::default()
    };
    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        relay_config,
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(1, tx, None).await;

    let event = event_builder!(Kind::TextNote, "hello").sign_with_keys(&keys)?;
    relay
        .handle_client_message(1, NostrClientMessage::event(event.clone()))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Ok { status, .. } => assert!(status),
        other => anyhow::bail!("expected OK, got {:?}", other),
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    let sub_id = SubscriptionId::new("sub-1");
    let filter = Filter::new()
        .authors(vec![event.pubkey])
        .kinds(vec![event.kind]);
    let mut got_event = false;
    for _ in 0..3 {
        relay
            .handle_client_message(
                1,
                NostrClientMessage::req(sub_id.clone(), vec![filter.clone()]),
            )
            .await;

        match recv_relay_message(&mut rx).await? {
            RelayMessage::Event {
                subscription_id,
                event: ev,
            } => {
                assert_eq!(subscription_id.as_ref(), &sub_id);
                assert_eq!(ev.id, event.id);
                got_event = true;
                break;
            }
            RelayMessage::EndOfStoredEvents(id) => {
                assert_eq!(id.as_ref(), &sub_id);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => anyhow::bail!("expected EVENT/EOSE, got {:?}", other),
        }
    }

    if !got_event {
        anyhow::bail!("event not available in time");
    }

    match recv_relay_message(&mut rx).await? {
        RelayMessage::EndOfStoredEvents(id) => assert_eq!(id.as_ref(), &sub_id),
        other => anyhow::bail!("expected EOSE, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_does_not_persist_ephemeral_events() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access.clone()),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let event = event_builder!(Kind::from_u16(25050), "").sign_with_keys(&keys)?;
    relay.ingest_trusted_event(event.clone()).await?;

    let filter = Filter::new()
        .authors(vec![event.pubkey])
        .kinds(vec![event.kind]);

    let reloaded = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    assert!(
        reloaded.query_events(&filter, 10).await.is_empty(),
        "ephemeral events should stay in memory only"
    );

    Ok(())
}

#[tokio::test]
async fn relay_persists_bluetooth_received_event_records() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access.clone()),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let cid = "cd".repeat(32);
    let event = event_builder!(
        Kind::TextNote,
        "bluetooth receipt",
        [nostr::Tag::parse(vec!["cid".to_string(), cid.clone()]).unwrap()],
    )
    .sign_with_keys(&keys)?;
    relay
        .ingest_trusted_event_from_bluetooth(event.clone(), Some("peer-a".to_string()))
        .await?;

    let receipts = relay.bluetooth_received_events(10).await;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].event_id, event.id.to_hex());
    assert_eq!(receipts[0].cid_values, vec![cid.clone()]);

    let reloaded = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;
    let reloaded_receipts = reloaded.bluetooth_received_events(10).await;
    assert_eq!(reloaded_receipts.len(), 1);
    assert_eq!(reloaded_receipts[0].event_id, event.id.to_hex());
    assert_eq!(reloaded_receipts[0].cid_values, vec![cid]);

    Ok(())
}

#[tokio::test]
async fn relay_persists_nhash_bluetooth_received_event_records() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access.clone()),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let nhash = hashtree_core::nhash_encode(&[0xef; 32])?;
    let event = event_builder!(
        Kind::TextNote,
        "bluetooth nhash receipt",
        [nostr::Tag::parse(vec!["cid".to_string(), nhash.clone()]).unwrap()],
    )
    .sign_with_keys(&keys)?;
    relay
        .ingest_trusted_event_from_bluetooth(event.clone(), Some("peer-a".to_string()))
        .await?;

    let receipts = relay.bluetooth_received_events(10).await;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].event_id, event.id.to_hex());
    assert_eq!(receipts[0].cid_values, vec![nhash.clone()]);

    let reloaded = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;
    let reloaded_receipts = reloaded.bluetooth_received_events(10).await;
    assert_eq!(reloaded_receipts.len(), 1);
    assert_eq!(reloaded_receipts[0].event_id, event.id.to_hex());
    assert_eq!(reloaded_receipts[0].cid_values, vec![nhash]);

    Ok(())
}

#[tokio::test]
async fn relay_caps_bluetooth_received_event_records_to_last_100() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let mut event_ids = Vec::new();
    for index in 0..(BLUETOOTH_EVENT_LOG_CAPACITY + 5) {
        let event = event_builder!(
            Kind::TextNote,
            format!("bluetooth receipt {index}"),
            [nostr::Tag::parse(vec!["cid".to_string(), format!("{index:064x}")]).unwrap()],
        )
        .sign_with_keys(&keys)?;
        event_ids.push(event.id.to_hex());
        relay
            .ingest_trusted_event_from_bluetooth(event, Some("peer-a".to_string()))
            .await?;
    }

    let receipts = relay
        .bluetooth_received_events(BLUETOOTH_EVENT_LOG_CAPACITY + 10)
        .await;
    assert_eq!(receipts.len(), BLUETOOTH_EVENT_LOG_CAPACITY);
    assert_eq!(receipts[0].event_id, event_ids.last().cloned().unwrap());
    assert!(
        receipts
            .iter()
            .all(|receipt| !event_ids[..5].contains(&receipt.event_id)),
        "oldest receipts should be trimmed from the capped log"
    );
    assert_eq!(
        receipts.last().map(|receipt| receipt.event_id.clone()),
        Some(event_ids[5].clone())
    );

    Ok(())
}

#[tokio::test]
async fn relay_serves_all_events_for_since_zero_catch_all_filter() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            max_query_limit: 10,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(8, tx, None).await;

    let first = event_builder!(Kind::TextNote, "first")
        .custom_created_at(nostr::Timestamp::from_secs(5))
        .sign_with_keys(&keys)?;
    let second = event_builder!(Kind::TextNote, "second")
        .custom_created_at(nostr::Timestamp::from_secs(6))
        .sign_with_keys(&keys)?;
    let third = event_builder!(Kind::TextNote, "third")
        .custom_created_at(nostr::Timestamp::from_secs(7))
        .sign_with_keys(&keys)?;

    for event in [&first, &second, &third] {
        relay
            .handle_client_message(8, NostrClientMessage::event(event.clone()))
            .await;
        match recv_relay_message(&mut rx).await? {
            RelayMessage::Ok { status, .. } => assert!(status),
            other => anyhow::bail!("expected OK, got {:?}", other),
        }
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    relay
        .handle_client_message(
            8,
            NostrClientMessage::from_json(r#"["REQ","sub-all",{"since":0}]"#)?,
        )
        .await;

    let mut received = HashSet::new();
    loop {
        match recv_relay_message(&mut rx).await? {
            RelayMessage::Event {
                subscription_id,
                event,
            } => {
                assert_eq!(subscription_id.as_ref(), &SubscriptionId::new("sub-all"));
                received.insert(event.id);
            }
            RelayMessage::EndOfStoredEvents(id) => {
                assert_eq!(id.as_ref(), &SubscriptionId::new("sub-all"));
                break;
            }
            other => anyhow::bail!("expected EVENT/EOSE, got {:?}", other),
        }
    }

    assert_eq!(received.len(), 3);
    assert_eq!(received, HashSet::from([first.id, second.id, third.id]));

    Ok(())
}

#[tokio::test]
async fn relay_spambox_does_not_serve_untrusted_events() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };

    crate::socialgraph::set_social_graph_root(&graph_store, &[1u8; 32]);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::new(),
    ));

    let relay_config = NostrRelayConfig {
        spambox_db_max_bytes: 0,
        ..Default::default()
    };
    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::new(),
        Some(access),
        relay_config,
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(2, tx, None).await;

    let keys = Keys::generate();
    let event = event_builder!(Kind::TextNote, "spam").sign_with_keys(&keys)?;
    relay
        .handle_client_message(2, NostrClientMessage::event(event.clone()))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Ok { status, .. } => assert!(status),
        other => anyhow::bail!("expected OK, got {:?}", other),
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    let sub_id = SubscriptionId::new("sub-2");
    let filter = Filter::new()
        .authors(vec![event.pubkey])
        .kinds(vec![event.kind]);
    relay
        .handle_client_message(2, NostrClientMessage::req(sub_id.clone(), vec![filter]))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::EndOfStoredEvents(id) => assert_eq!(id.as_ref(), &sub_id),
        other => anyhow::bail!("expected EOSE only, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_trusts_authenticated_client_for_its_own_events() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };

    crate::socialgraph::set_social_graph_root(&graph_store, &[1u8; 32]);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::new(),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::new(),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let keys = Keys::generate();
    relay
        .register_client(9, tx, Some(keys.public_key().to_hex()))
        .await;

    let event = event_builder!(Kind::TextNote, "self-authored").sign_with_keys(&keys)?;
    relay
        .handle_client_message(9, NostrClientMessage::event(event.clone()))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Ok {
            status, message, ..
        } => {
            assert!(status);
            assert_eq!(message, "");
        }
        other => anyhow::bail!("expected OK, got {:?}", other),
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    let sub_id = SubscriptionId::new("sub-auth");
    let filter = Filter::new()
        .authors(vec![event.pubkey])
        .kinds(vec![event.kind]);
    relay
        .handle_client_message(9, NostrClientMessage::req(sub_id.clone(), vec![filter]))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Event {
            subscription_id,
            event: stored,
        } => {
            assert_eq!(subscription_id.as_ref(), &sub_id);
            assert_eq!(stored.id, event.id);
        }
        other => anyhow::bail!("expected EVENT, got {:?}", other),
    }

    match recv_relay_message(&mut rx).await? {
        RelayMessage::EndOfStoredEvents(id) => assert_eq!(id.as_ref(), &sub_id),
        other => anyhow::bail!("expected EOSE, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_routes_non_authored_trusted_events_to_ambient_index() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let authored_keys = Keys::generate();
    let remote_keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([authored_keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([authored_keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let ambient_event = event_builder!(Kind::TextNote, "ambient")
        .custom_created_at(nostr::Timestamp::from_secs(5))
        .sign_with_keys(&remote_keys)?;
    relay.ingest_trusted_event(ambient_event.clone()).await?;

    let filter = Filter::new()
        .author(remote_keys.public_key())
        .kind(Kind::TextNote);
    let ambient_only = graph_store
        .query_events_in_scope(
            &filter,
            10,
            crate::socialgraph::EventQueryScope::AmbientOnly,
        )
        .unwrap();
    assert_eq!(ambient_only.len(), 1);
    assert_eq!(ambient_only[0].id, ambient_event.id);

    let public_only = graph_store
        .query_events_in_scope(&filter, 10, crate::socialgraph::EventQueryScope::PublicOnly)
        .unwrap();
    assert!(public_only.is_empty());

    Ok(())
}

#[tokio::test]
async fn relay_serves_parameterized_replaceable_queries() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let mut allowed = HashSet::new();
    allowed.insert(keys.public_key().to_hex());
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        allowed,
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(3, tx, None).await;

    let older = event_builder!(
        Kind::Custom(30078),
        "",
        vec![
            nostr::Tag::identifier("video"),
            nostr::Tag::parse(["l", "hashtree"])?,
            nostr::Tag::parse(vec!["hash".to_string(), "11".repeat(32)])?,
        ],
    )
    .custom_created_at(nostr::Timestamp::from_secs(5))
    .sign_with_keys(&keys)?;
    let newer = event_builder!(
        Kind::Custom(30078),
        "",
        vec![
            nostr::Tag::identifier("video"),
            nostr::Tag::parse(["l", "hashtree"])?,
            nostr::Tag::parse(vec!["hash".to_string(), "22".repeat(32)])?,
        ],
    )
    .custom_created_at(nostr::Timestamp::from_secs(6))
    .sign_with_keys(&keys)?;

    relay
        .handle_client_message(3, NostrClientMessage::event(older.clone()))
        .await;
    let _ = recv_relay_message(&mut rx).await?;
    relay
        .handle_client_message(3, NostrClientMessage::event(newer.clone()))
        .await;
    let _ = recv_relay_message(&mut rx).await?;

    let sub_id = SubscriptionId::new("sub-d");
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Custom(30078))
        .identifier("video");
    relay
        .handle_client_message(3, NostrClientMessage::req(sub_id.clone(), vec![filter]))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Event {
            subscription_id,
            event,
        } => {
            assert_eq!(subscription_id.as_ref(), &sub_id);
            assert_eq!(event.id, newer.id);
        }
        other => anyhow::bail!("expected EVENT, got {:?}", other),
    }

    match recv_relay_message(&mut rx).await? {
        RelayMessage::EndOfStoredEvents(id) => assert_eq!(id.as_ref(), &sub_id),
        other => anyhow::bail!("expected EOSE, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_serves_replaceable_queries() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(4, tx, None).await;

    let older = event_builder!(Kind::Metadata, r#"{"name":"older"}"#)
        .custom_created_at(nostr::Timestamp::from_secs(5))
        .sign_with_keys(&keys)?;
    let newer = event_builder!(Kind::Metadata, r#"{"name":"newer"}"#)
        .custom_created_at(nostr::Timestamp::from_secs(6))
        .sign_with_keys(&keys)?;

    relay
        .handle_client_message(4, NostrClientMessage::event(older.clone()))
        .await;
    let _ = recv_relay_message(&mut rx).await?;
    relay
        .handle_client_message(4, NostrClientMessage::event(newer.clone()))
        .await;
    let _ = recv_relay_message(&mut rx).await?;

    let sub_id = SubscriptionId::new("sub-profile");
    let filter = Filter::new().author(keys.public_key()).kind(Kind::Metadata);
    relay
        .handle_client_message(4, NostrClientMessage::req(sub_id.clone(), vec![filter]))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Event {
            subscription_id,
            event,
        } => {
            assert_eq!(subscription_id.as_ref(), &sub_id);
            assert_eq!(event.id, newer.id);
        }
        other => anyhow::bail!("expected EVENT, got {:?}", other),
    }

    match recv_relay_message(&mut rx).await? {
        RelayMessage::EndOfStoredEvents(id) => assert_eq!(id.as_ref(), &sub_id),
        other => anyhow::bail!("expected EOSE, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_count_dedupes_across_filters_and_honors_filter_limits() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(5, tx, None).await;

    let older = event_builder!(Kind::Metadata, r#"{"name":"older"}"#)
        .custom_created_at(nostr::Timestamp::from_secs(5))
        .sign_with_keys(&keys)?;
    let newer = event_builder!(Kind::Metadata, r#"{"name":"newer"}"#)
        .custom_created_at(nostr::Timestamp::from_secs(6))
        .sign_with_keys(&keys)?;

    relay
        .handle_client_message(5, NostrClientMessage::event(older.clone()))
        .await;
    let _ = recv_relay_message(&mut rx).await?;
    relay
        .handle_client_message(5, NostrClientMessage::event(newer.clone()))
        .await;
    let _ = recv_relay_message(&mut rx).await?;

    let sub_id = SubscriptionId::new("sub-count");
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(10);
    relay
        .handle_client_message(5, NostrClientMessage::count(sub_id.clone(), filter))
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Count {
            subscription_id,
            count,
        } => {
            assert_eq!(subscription_id.as_ref(), &sub_id);
            assert_eq!(count, 1);
        }
        other => anyhow::bail!("expected COUNT, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_count_caps_filter_limit_to_config_max() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            max_query_limit: 1,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(7, tx, None).await;

    let older = event_builder!(Kind::TextNote, "older")
        .custom_created_at(nostr::Timestamp::from_secs(5))
        .sign_with_keys(&keys)?;
    let newer = event_builder!(Kind::TextNote, "newer")
        .custom_created_at(nostr::Timestamp::from_secs(6))
        .sign_with_keys(&keys)?;

    relay
        .handle_client_message(7, NostrClientMessage::event(older))
        .await;
    let _ = recv_relay_message(&mut rx).await?;
    relay
        .handle_client_message(7, NostrClientMessage::event(newer))
        .await;
    let _ = recv_relay_message(&mut rx).await?;

    relay
        .handle_client_message(
            7,
            NostrClientMessage::count(
                SubscriptionId::new("sub-count-cap"),
                Filter::new()
                    .author(keys.public_key())
                    .kind(Kind::TextNote)
                    .limit(10),
            ),
        )
        .await;

    match recv_relay_message(&mut rx).await? {
        RelayMessage::Count { count, .. } => assert_eq!(count, 1),
        other => anyhow::bail!("expected COUNT, got {:?}", other),
    }

    Ok(())
}

#[tokio::test]
async fn relay_register_subscription_query_caps_filter_limit_to_config_max() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock();
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let keys = Keys::generate();
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::from([keys.public_key().to_hex()]),
    ));

    let relay = NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            max_query_limit: 1,
            ..Default::default()
        },
    )?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    relay.register_client(6, tx, None).await;

    let older = event_builder!(Kind::TextNote, "older")
        .custom_created_at(nostr::Timestamp::from_secs(5))
        .sign_with_keys(&keys)?;
    let newer = event_builder!(Kind::TextNote, "newer")
        .custom_created_at(nostr::Timestamp::from_secs(6))
        .sign_with_keys(&keys)?;

    relay
        .handle_client_message(6, NostrClientMessage::event(older))
        .await;
    let _ = recv_relay_message(&mut rx).await?;
    relay
        .handle_client_message(6, NostrClientMessage::event(newer))
        .await;
    let _ = recv_relay_message(&mut rx).await?;

    let events = relay
        .register_subscription_query(
            6,
            SubscriptionId::new("sub-limit"),
            vec![Filter::new()
                .author(keys.public_key())
                .kind(Kind::TextNote)
                .limit(10)],
        )
        .await
        .map_err(anyhow::Error::msg)?;

    assert_eq!(events.len(), 1);

    Ok(())
}
