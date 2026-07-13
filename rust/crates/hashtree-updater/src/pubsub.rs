use hashtree_resolver::nostr::NostrRootResolver;
use hashtree_resolver::Event;
use nostr_pubsub::{EventBus, Filter, MatchEventOptions, QueryOptions, VerifiedEvent};

use crate::{UpdateError, UpdateRef};

const UPDATE_QUERY_LIMIT: usize = 8;

/// The newest signed Hashtree root event for one app release tree.
///
/// Transport adapters feed this cache ordinary `nostr-pubsub` events. The
/// exact author/tree filter is derived from the trusted [`UpdateRef`], so an
/// event from another publisher can never redirect an update check.
#[derive(Debug, Clone)]
pub struct UpdateEventCache {
    resolver_key: String,
    filter: Filter,
    latest: Option<VerifiedEvent>,
}

impl UpdateEventCache {
    pub fn new(reference: &UpdateRef) -> Result<Self, UpdateError> {
        let resolver_key = reference.resolver_key();
        Ok(Self {
            filter: NostrRootResolver::filter_for_key(&resolver_key)?,
            resolver_key,
            latest: None,
        })
    }

    #[must_use]
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    #[must_use]
    pub fn latest(&self) -> Option<&VerifiedEvent> {
        self.latest.as_ref()
    }

    /// Accept a verified event only if it advances this release tree.
    pub fn ingest(&mut self, event: VerifiedEvent) -> bool {
        if !self
            .filter
            .match_event(event.as_event(), MatchEventOptions::new())
            || !matches!(
                NostrRootResolver::event_matches_key(&self.resolver_key, event.as_event()),
                Ok(true)
            )
            || self.latest.as_ref().is_some_and(|current| {
                (event.as_event().created_at, event.as_event().id)
                    <= (current.as_event().created_at, current.as_event().id)
            })
        {
            return false;
        }
        self.latest = Some(event);
        true
    }

    pub fn ingest_event(&mut self, event: Event) -> Result<bool, UpdateError> {
        let event = VerifiedEvent::try_from(event)
            .map_err(|error| UpdateError::Announcement(error.to_string()))?;
        Ok(self.ingest(event))
    }

    /// Query a selected pubsub provider and retain the newest matching event.
    pub async fn refresh<P>(&mut self, provider: &P) -> Result<bool, UpdateError>
    where
        P: EventBus + ?Sized,
    {
        let report = provider
            .query(
                vec![self.filter.clone()],
                QueryOptions {
                    limit: Some(UPDATE_QUERY_LIMIT),
                },
            )
            .await
            .map_err(|error| UpdateError::Announcement(error.to_string()))?;
        let mut advanced = false;
        for event in report.events {
            advanced |= self.ingest(event.event);
        }
        Ok(advanced)
    }

    /// Signed events suitable for seeding `NostrRootResolver` without relays.
    #[must_use]
    pub fn resolver_events(&self) -> Vec<Event> {
        self.latest
            .as_ref()
            .map(|event| vec![event.as_event().clone()])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use nostr_pubsub::{EventSource, InMemoryEventBus};
    use nostr_sdk::{Alphabet, EventBuilder, Keys, Kind, SingleLetterTag, Tag, TagKind, ToBech32};

    use super::*;

    fn update_ref(keys: &Keys, tree_name: &str) -> UpdateRef {
        UpdateRef {
            npub: keys.public_key().to_bech32().expect("npub"),
            tree_name: tree_name.to_string(),
            path: Some("latest".to_string()),
        }
    }

    fn root_event(keys: &Keys, tree_name: &str, created_at: u64, byte: u8) -> Event {
        EventBuilder::new(Kind::Custom(30_064), "")
            .tags([
                Tag::identifier(tree_name),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    ["hashtree"],
                ),
                Tag::custom(
                    TagKind::Custom("hash".into()),
                    [format!("{byte:02x}").repeat(32)],
                ),
            ])
            .custom_created_at(nostr_sdk::Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("signed root")
    }

    #[test]
    fn cache_accepts_only_newer_events_for_the_trusted_release_tree() {
        let release_keys = Keys::generate();
        let other_keys = Keys::generate();
        let reference = update_ref(&release_keys, "iris/releases");
        let mut cache = UpdateEventCache::new(&reference).expect("cache");

        assert!(!cache
            .ingest_event(root_event(&other_keys, "iris/releases", 3, 3))
            .expect("valid signed event"));
        assert!(!cache
            .ingest_event(root_event(&release_keys, "other", 3, 3))
            .expect("valid signed event"));
        let malformed = EventBuilder::new(Kind::Custom(30_064), "")
            .tags([Tag::identifier("iris/releases")])
            .custom_created_at(nostr_sdk::Timestamp::from(4))
            .sign_with_keys(&release_keys)
            .expect("signed malformed root");
        assert!(!cache
            .ingest_event(malformed)
            .expect("valid signature is still not a root"));
        assert!(cache
            .ingest_event(root_event(&release_keys, "iris/releases", 2, 2))
            .expect("valid signed event"));
        assert!(!cache
            .ingest_event(root_event(&release_keys, "iris/releases", 1, 1))
            .expect("valid signed event"));
        assert!(cache
            .ingest_event(root_event(&release_keys, "iris/releases", 3, 3))
            .expect("valid signed event"));

        assert_eq!(
            cache
                .latest()
                .expect("latest")
                .as_event()
                .created_at
                .as_secs(),
            3
        );
        assert_eq!(cache.resolver_events().len(), 1);
    }

    #[tokio::test]
    async fn refresh_queries_nostr_pubsub_and_coalesces_the_latest_root() {
        let release_keys = Keys::generate();
        let reference = update_ref(&release_keys, "iris/releases");
        let bus = InMemoryEventBus::new();
        for (created_at, byte) in [(4, 4), (6, 6), (5, 5)] {
            bus.publish(
                VerifiedEvent::try_from(root_event(
                    &release_keys,
                    "iris/releases",
                    created_at,
                    byte,
                ))
                .expect("verified root"),
                EventSource::peer("release-peer"),
            )
            .await
            .expect("publish root");
        }

        let mut cache = UpdateEventCache::new(&reference).expect("cache");
        assert!(cache.refresh(&bus).await.expect("pubsub query"));
        assert_eq!(
            cache
                .latest()
                .expect("latest")
                .as_event()
                .created_at
                .as_secs(),
            6
        );
    }
}
