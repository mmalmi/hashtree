//! Host-configured read sources for exact public tree names.
use super::auth::AppState;
use crate::config::NostrEventTransport;
use nostr::nips::nip19::ToBech32;
use nostr::{Event, Filter, RelayUrl};
use nostr_pubsub::{EventBus, PubsubProviderMode, QueryOptions};
use nostr_pubsub_relay::RelayEventBus;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

#[derive(Clone, Default)]
pub(super) struct RootReadRelays {
    roots: HashMap<String, Arc<ReadRelay>>,
}

struct ReadRelay {
    url: RelayUrl,
    client: OnceCell<RelayEventBus>,
}

impl RootReadRelays {
    pub(super) fn new(roots: &BTreeMap<String, String>) -> Self {
        let mut sources: HashMap<RelayUrl, Arc<ReadRelay>> = HashMap::new();
        let mut result = Self::default();
        for (name, configured) in roots {
            let Ok(url) = RelayUrl::parse(configured) else {
                continue;
            };
            let source = sources.entry(url.clone()).or_insert_with(|| {
                Arc::new(ReadRelay {
                    url,
                    client: OnceCell::new(),
                })
            });
            result.roots.insert(name.clone(), Arc::clone(source));
        }
        result
    }

    pub(super) async fn query(
        &self,
        state: &AppState,
        name: &str,
        filter: &Filter,
    ) -> Option<Event> {
        if self.roots.is_empty()
            || state.nostr_relay_urls.is_empty()
            || state.nostr_event_transport != Some(NostrEventTransport::Relay)
            || state
                .nostr_provider
                .as_ref()
                .is_some_and(|provider| provider.mode() == PubsubProviderMode::LocalOnly)
        {
            return None;
        }
        // The caller builds this filter from one author and one tree. The HTTP request
        // cannot supply a destination, and only the configured public name may leave.
        let author = filter.authors.as_ref()?.first()?.to_bech32().ok()?;
        let source = self.roots.get(&format!("{author}/{name}"))?;
        let bus = source
            .client
            .get_or_try_init(|| {
                RelayEventBus::with_client(
                    nostr_sdk::Client::default(),
                    [source.url.to_string()],
                    Duration::from_secs(4),
                )
            })
            .await
            .ok()?;
        let result = bus
            .query(vec![filter.clone()], QueryOptions { limit: Some(1) })
            .await
            .ok()?;
        let event = result.events.into_iter().next()?.event.into_event();
        if let Some(local) = &state.nostr_relay {
            let _ = local.ingest_trusted_event_silent(event.clone()).await;
        }
        Some(event)
    }
}
