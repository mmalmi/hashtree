use super::*;

impl NostrRootResolver {
    /// Observe a mutable root for a bounded window, keeping the subscription open
    /// after EOSE. Returns the newest usable signed root observed in that window;
    /// a quiet window is a timeout, not proof that the root does not exist.
    /// Unlike quick resolution, this does not use cached observations or a quorum
    /// shortcut. It closes only its own subscription before returning.
    pub async fn resolve_open(&self, key: &str, window: Duration) -> Result<Cid, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;
        if self.config.relays.is_empty() {
            return Err(ResolverError::Network(
                "No active Nostr relays configured".into(),
            ));
        }
        let filter = Self::build_tree_filter(pubkey, &tree_name);
        let id = SubscriptionId::generate();
        // Register before REQ so even an immediate relay response is observed.
        let mut notifications = self.client.notifications();
        let mut latest: Option<(VerifiedEvent, Cid)> = None;
        let observed: Result<Result<(), ResolverError>, _> = tokio::time::timeout(window, async {
            self.client
                .subscribe_with_id(id.clone(), filter, None)
                .await
                .map_err(|error| ResolverError::Network(error.to_string()))?;
            loop {
                let notification = notifications
                    .recv()
                    .await
                    .map_err(|error| ResolverError::Network(error.to_string()))?;
                // Event notifications are globally deduplicated by the SDK. Raw
                // messages also cover a root already seen on another subscription.
                let RelayPoolNotification::Message {
                    message:
                        RelayMessage::Event {
                            subscription_id,
                            event,
                        },
                    ..
                } = notification
                else {
                    continue;
                };
                if subscription_id.as_ref() != &id {
                    continue;
                }
                let Ok(event) = VerifiedEvent::try_from(event.into_owned()) else {
                    continue;
                };
                if event.as_event().pubkey != pubkey || !is_matching_tree_event(&event, &tree_name)
                {
                    continue;
                }
                let Some(cid) = self.cid_from_event(event.as_event()) else {
                    continue;
                };
                if latest.as_ref().is_none_or(|(current, _)| {
                    is_newer_event(event.as_event(), current.created_at(), Some(current.id()))
                }) {
                    latest = Some((event, cid));
                }
            }
        })
        .await;
        self.client.unsubscribe(&id).await;
        if let Ok(result) = observed {
            result?;
        }
        latest.map(|(_, cid)| cid).ok_or_else(|| {
            ResolverError::Network(format!("Timed out waiting for a signed root for {key}"))
        })
    }
}
