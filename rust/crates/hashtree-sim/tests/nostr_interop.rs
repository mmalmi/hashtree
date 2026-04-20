#[cfg(feature = "nostr")]
mod nostr_interop {
    use hashtree_core;
    use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
    use hashtree_resolver::RootResolver;
    use hashtree_sim::WsRelay;
    use nostr_sdk::prelude::*;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn unique_tree_name(prefix: &str) -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_millis();
        format!("{prefix}-{ts}")
    }

    async fn publish_event(
        relay_url: &str,
        keys: &Keys,
        tags: Vec<Tag>,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client = Client::new(keys.clone());
        client.add_relay(relay_url).await?;
        client.connect().await;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let event = EventBuilder::new(Kind::Custom(30078), content, tags.clone());
            match client.send_event_builder(event).await {
                Ok(_) => break,
                Err(err) if Instant::now() < deadline => {
                    let _ = err;
                    tokio::task::yield_now().await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    async fn wait_for_relay_events(relay: &WsRelay, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if relay.event_count().await >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "relay did not receive {expected} event(s) in time"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn resolves_tagged_event_from_ts() {
        let mut relay = WsRelay::new();
        relay.start().await.expect("start relay");
        let relay_url = relay.url().expect("relay url");

        let keys = Keys::generate();
        let tree_name = unique_tree_name("ts-tagged");
        let hash_hex = "1234".repeat(16);

        let tags = vec![
            Tag::identifier(tree_name.clone()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![hash_hex.clone()]),
        ];

        publish_event(&relay_url, &keys, tags, "")
            .await
            .expect("publish");
        wait_for_relay_events(&relay, 1).await;

        let resolver = NostrRootResolver::new(NostrResolverConfig {
            relays: vec![relay_url],
            resolve_timeout: Duration::from_secs(2),
            secret_key: None,
        })
        .await
        .expect("resolver");

        let npub = keys.public_key().to_bech32().expect("npub");
        let key = format!("{npub}/{tree_name}");
        let resolved = resolver.resolve(&key).await.expect("resolve");

        let cid = resolved.expect("cid");
        assert_eq!(hashtree_core::to_hex(&cid.hash), hash_hex);

        relay.stop().await;
    }

    #[tokio::test]
    async fn resolves_legacy_content_event_without_label() {
        let mut relay = WsRelay::new();
        relay.start().await.expect("start relay");
        let relay_url = relay.url().expect("relay url");

        let keys = Keys::generate();
        let tree_name = unique_tree_name("legacy-content");
        let hash_hex = "abcd".repeat(16);
        let content = format!(r#"{{"hash":"{hash_hex}"}}"#);

        let tags = vec![Tag::identifier(tree_name.clone())];

        publish_event(&relay_url, &keys, tags, &content)
            .await
            .expect("publish");
        wait_for_relay_events(&relay, 1).await;

        let resolver = NostrRootResolver::new(NostrResolverConfig {
            relays: vec![relay_url],
            resolve_timeout: Duration::from_secs(2),
            secret_key: None,
        })
        .await
        .expect("resolver");

        let npub = keys.public_key().to_bech32().expect("npub");
        let key = format!("{npub}/{tree_name}");
        let resolved = resolver.resolve(&key).await.expect("resolve");

        let cid = resolved.expect("cid");
        assert_eq!(hashtree_core::to_hex(&cid.hash), hash_hex);

        relay.stop().await;
    }
}
